//! Modality adaptation layer — translates non-text modalities to text
//! when the primary chat model does not support them natively.
//!
//! Operates on cloned messages only (never mutates persistent history).
//! Translation results are cached by content fingerprint so historical
//! media can reuse a description instead of degrading to a placeholder.
//!
//! The module is modality-driven: each modality is described by a
//! [`ModalitySpec`] (how to match parts, which prompt to send, the
//! placeholder text). The core detect/replace/translate logic is written
//! once; audio/video support only adds a new spec plus a `part_matches`
//! branch.

use crate::providers::capability::Modality;
use crate::providers::capability_chat::{
    ChatMessage, ChatProvider, ChatRequest, ChatResponse, ContentPart,
};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

// ── Modality spec + media fingerprint (T2.1) ──────────────────────────────────

/// Static description of how to adapt one modality.
pub struct ModalitySpec {
    /// The input modality this spec adapts.
    pub modality: Modality,
    /// Prompt sent to the auxiliary model.
    pub prompt: &'static str,
    /// Label for the injected description, e.g. "图片" / "音频" / "视频".
    pub label: &'static str,
    /// Placeholder used when no description is available, e.g. "[image]".
    pub placeholder: &'static str,
}

/// Image adaptation spec — describe an image in detail.
pub const IMAGE_SPEC: ModalitySpec = ModalitySpec {
    modality: Modality::Image,
    prompt: "Describe this image in detail, including any text, objects, \
             layout, and notable visual information.",
    label: "图片",
    placeholder: "[image]",
};
// Phase 2/3: const AUDIO_SPEC / VIDEO_SPEC — same struct, different fields.

/// Whether a part carries media of the given modality.
pub fn part_matches(part: &ContentPart, modality: &Modality) -> bool {
    match modality {
        Modality::Image => matches!(
            part,
            ContentPart::ImageUrl { .. } | ContentPart::ImageB64 { .. }
        ),
        // Phase 2/3: Audio / Video parts once ContentPart gains them.
        _ => false,
    }
}

/// Content fingerprint used as the description-cache key — the part's
/// content-addressed identity (decoded-bytes SHA-256 for images, invariant
/// across `ImageB64 ⇄ ImageRef` and matching `blobs/{hash}.bin`). Delegates to
/// [`ContentPart::content_fingerprint`] so the cache key, the blob filename, and
/// the description-sweep key are all the same value. `None` for non-media parts.
pub fn fingerprint(part: &ContentPart) -> Option<String> {
    part.content_fingerprint()
}

// ── Description cache (T2.2) ───────────────────────────────────────────────────

/// Cache: `(session_id, content fingerprint)` → text description.
///
/// `session_id`-scoped so a persisted description lives alongside the session's
/// image blobs and is reclaimed with the session (see [`PersistentDescriptionCache`]).
/// The key is a content sha256, so within a session a description is
/// content-addressed and never needs explicit invalidation (the same media
/// content always yields the same description).
pub trait DescriptionCache: Send + Sync {
    /// Look up a cached description for `session_id` by fingerprint key.
    fn get(&self, session_id: &str, key: &str) -> Option<String>;
    /// Store a description for `session_id` under a fingerprint key.
    fn put(&self, session_id: &str, key: String, value: String);
}

/// In-memory `(session, key)` composite for the LRU tiers. Sessions are isolated
/// (no cross-session sharing) so each session persists its own descriptions.
fn hot_key(session_id: &str, key: &str) -> String {
    // NUL separator: never appears in a session id or a sha256 hex key.
    format!("{session_id}\0{key}")
}

/// LRU-backed in-memory [`DescriptionCache`]. Used by CLI one-shot commands and
/// tests, where cross-restart persistence is unnecessary.
pub struct LruDescriptionCache {
    inner: Mutex<lru::LruCache<String, String>>,
}

impl LruDescriptionCache {
    /// Build a cache holding up to `capacity` descriptions (clamped to >= 1).
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity.max(1)).expect("capacity >= 1");
        Self {
            inner: Mutex::new(lru::LruCache::new(cap)),
        }
    }
}

impl Default for LruDescriptionCache {
    fn default() -> Self {
        // A few hundred descriptions; each value is short text — negligible memory.
        Self::new(512)
    }
}

impl DescriptionCache for LruDescriptionCache {
    fn get(&self, session_id: &str, key: &str) -> Option<String> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.get(&hot_key(session_id, key)).cloned()
    }

    fn put(&self, session_id: &str, key: String, value: String) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.put(hot_key(session_id, &key), value);
    }
}

/// Two-tier [`DescriptionCache`]: a bounded in-memory LRU hot tier backed by a
/// per-session, content-addressed on-disk cold tier. Descriptions live next to
/// the session's image blobs at `{sessions_root}/{session_id}/descriptions/{key}.txt`
/// (a sibling of `.../blobs/`), so they:
///
/// * **survive restarts and hot-tier eviction** — a non-vision model recovers
///   historical-image descriptions without re-invoking the auxiliary model, just
///   as `storage::json_file` re-hydrates the image bytes from blobs; and
/// * **are reclaimed with the session** — deleting a session `remove_dir_all`s
///   its directory, taking blobs *and* descriptions with it (no global orphans).
///
/// Within a live session the cold tier is unbounded (each entry is a few KB of
/// content-addressed text — slow growth). It is not wired into the per-rotation
/// blob mark-and-sweep: that sweep is keyed by the decoded-bytes blob hash, while
/// descriptions are keyed by the b64 fingerprint, and the b64 is not on disk after
/// externalization. Session deletion is the reclamation point.
pub struct PersistentDescriptionCache {
    hot: Mutex<lru::LruCache<String, String>>,
    sessions_root: PathBuf,
}

impl PersistentDescriptionCache {
    /// Build over the sessions root (the directory that holds per-session
    /// subdirectories — the same path passed to the session backend), with a
    /// `capacity`-entry in-memory hot tier (clamped to >= 1). Per-session cold-tier
    /// directories are created lazily on first write.
    pub fn open(sessions_root: impl Into<PathBuf>, capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity.max(1)).expect("capacity >= 1");
        Self {
            hot: Mutex::new(lru::LruCache::new(cap)),
            sessions_root: sessions_root.into(),
        }
    }

    /// Per-session cold-tier directory (sibling of the session's `blobs/`).
    fn dir_for(&self, session_id: &str) -> PathBuf {
        self.sessions_root.join(session_id).join("descriptions")
    }

    /// Cold-tier file path for `(session_id, key)`. The key is a sha256 hex
    /// string, so it is always a safe single-segment filename.
    fn path_for(&self, session_id: &str, key: &str) -> PathBuf {
        self.dir_for(session_id).join(format!("{key}.txt"))
    }
}

impl DescriptionCache for PersistentDescriptionCache {
    fn get(&self, session_id: &str, key: &str) -> Option<String> {
        let hk = hot_key(session_id, key);
        {
            let mut guard = self.hot.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(hit) = guard.get(&hk) {
                return Some(hit.clone());
            }
        }
        // Cold-tier read-through: a hit warms the hot tier for next time.
        let text = std::fs::read_to_string(self.path_for(session_id, key)).ok()?;
        let mut guard = self.hot.lock().unwrap_or_else(|e| e.into_inner());
        guard.put(hk, text.clone());
        Some(text)
    }

    fn put(&self, session_id: &str, key: String, value: String) {
        {
            let mut guard = self.hot.lock().unwrap_or_else(|e| e.into_inner());
            guard.put(hot_key(session_id, &key), value.clone());
        }
        // Write-through, atomic (temp + rename) so a partial write can never be
        // read back as a truncated description. The per-session dir is created lazily.
        let path = self.path_for(session_id, &key);
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!(
                    path = %parent.display(), err = %e,
                    "failed to create description dir; hot-tier only"
                );
                return;
            }
        }
        let tmp = path.with_extension("tmp");
        let written =
            std::fs::write(&tmp, value.as_bytes()).and_then(|_| std::fs::rename(&tmp, &path));
        if let Err(e) = written {
            tracing::warn!(
                path = %path.display(), err = %e,
                "failed to persist description; hot-tier only"
            );
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

// ── Auxiliary translation (T2.4) ──────────────────────────────────────────────

/// Translate one media part to text using an auxiliary model.
/// Returns the cached description on hit; otherwise performs a single,
/// self-contained (history-free) streaming chat call and caches the result.
pub async fn translate_part(
    provider: &dyn ChatProvider,
    model_id: &str,
    part: &ContentPart,
    spec: &ModalitySpec,
    cache: &dyn DescriptionCache,
    session_id: &str,
) -> anyhow::Result<String> {
    if let Some(key) = fingerprint(part) {
        if let Some(hit) = cache.get(session_id, &key) {
            return Ok(hit);
        }
    }

    let user_msg = ChatMessage {
        role: "user".into(),
        parts: vec![part.clone(), ContentPart::Text { text: spec.prompt.into() }],
        name: None,
        tool_call_id: None,
        tool_calls: None,
        is_error: None,
    };

    let messages = [user_msg];
    let req = ChatRequest {
        model: model_id,
        messages: &messages,
        temperature: Some(0.3),
        max_tokens: Some(1024),
        thinking: None,
        stop: None,
        seed: None,
        tools: None,
        stream: true, // convention: always true; caller must not set false
    };

    // chat() returns a BoxStream that must be aggregated into a full response.
    let stream = provider.chat(req)?;
    let resp = ChatResponse::from_stream(stream).await?;
    let text = resp.text;

    if let Some(key) = fingerprint(part) {
        cache.put(session_id, key, text.clone());
    }
    Ok(text)
}

// ── Historical media adaptation (T2.5) ────────────────────────────────────────

/// Replace historical media parts (everything except the current turn at
/// `skip_idx`, which is handled by [`adapt_last_turn_media`]). Reuses a cached
/// description when available so follow-up questions still work; otherwise
/// degrades to the placeholder. Never calls the auxiliary model — no new
/// translation for stale context (cache hits are free, misses are not worth a
/// round-trip).
pub fn adapt_history_media(
    messages: &mut [ChatMessage],
    spec: &ModalitySpec,
    cache: &dyn DescriptionCache,
    session_id: &str,
    skip_idx: Option<usize>,
) {
    for (i, msg) in messages.iter_mut().enumerate() {
        if Some(i) == skip_idx {
            continue; // current-turn message handled by adapt_last_turn_media
        }
        for part in msg.parts.iter_mut() {
            if !part_matches(part, &spec.modality) {
                continue;
            }
            let replacement = fingerprint(part)
                .and_then(|k| cache.get(session_id, &k))
                .map(|desc| format!("[{}描述]: {}", spec.label, desc))
                .unwrap_or_else(|| spec.placeholder.to_string());
            *part = ContentPart::Text { text: replacement };
        }
    }
}

// ── Current-turn media adaptation (T2.6) ──────────────────────────────────────

/// Adapt the current-turn media in a single user message: translate this
/// message's media parts of the spec's modality (in parallel) and replace them
/// with one combined text description part (numbered when more than one).
/// Non-media parts (e.g. the user's own text) are preserved in order.
///
/// Degrades gracefully — on `aux == None` it emits a placeholder, and a
/// per-part translation error becomes `"[translation failed]"`. Never panics
/// and never mutates persistent history (operates on a cloned message).
pub async fn adapt_last_turn_media(
    msg: &mut ChatMessage,
    spec: &ModalitySpec,
    aux: Option<(&Arc<dyn ChatProvider>, &str)>,
    cache: &dyn DescriptionCache,
    session_id: &str,
) {
    // Collect this message's media parts of the target modality.
    let media: Vec<ContentPart> = msg
        .parts
        .iter()
        .filter(|p| part_matches(p, &spec.modality))
        .cloned()
        .collect();
    if media.is_empty() {
        return;
    }

    let description_part = match aux {
        None => ContentPart::Text {
            text: format!("[{} — no {} model available]", spec.placeholder, spec.modality.as_str()),
        },
        Some((provider, model_id)) => {
            // Parallel translation — multiple media don't serialize latency.
            let futs = media
                .iter()
                .map(|part| translate_part(provider.as_ref(), model_id, part, spec, cache, session_id));
            let results = futures_util::future::join_all(futs).await;

            let descriptions: Vec<String> = results
                .into_iter()
                .map(|r| match r {
                    Ok(desc) => desc,
                    Err(e) => {
                        tracing::warn!(
                            err = %e,
                            modality = ?spec.modality,
                            "auxiliary translation failed"
                        );
                        "[translation failed]".to_string()
                    }
                })
                .collect();

            let text = if descriptions.len() == 1 {
                format!("[{}描述]: {}", spec.label, descriptions[0])
            } else {
                descriptions
                    .iter()
                    .enumerate()
                    .map(|(i, d)| format!("[{}{}描述]: {}", spec.label, i + 1, d))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            ContentPart::Text { text }
        }
    };

    // Replace the media parts in place with the single description part:
    // keep all non-media parts in their original order; splice the
    // description where the first media part was (or append if none precede).
    let mut new_parts: Vec<ContentPart> = Vec::with_capacity(msg.parts.len());
    let mut injected = false;
    for part in msg.parts.drain(..) {
        if part_matches(&part, &spec.modality) {
            if !injected {
                new_parts.push(description_part.clone());
                injected = true;
            }
            // drop the media part
        } else {
            new_parts.push(part);
        }
    }
    msg.parts = new_parts;
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::capability_chat::{
        BoxStream, ImageDetail, StopReason, StreamEvent,
    };
    use futures_util::stream;

    /// Mock provider returning a fixed text delta stream, recording how many
    /// times it was invoked (to assert cache hits skip the call).
    struct MockProvider {
        reply: String,
        calls: Arc<Mutex<usize>>,
    }

    #[async_trait::async_trait]
    impl ChatProvider for MockProvider {
        fn chat(&self, _req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
            *self.calls.lock().unwrap() += 1;
            let reply = self.reply.clone();
            let events = vec![
                StreamEvent::Delta { text: reply },
                StreamEvent::Done { reason: StopReason::EndTurn },
            ];
            Ok(Box::pin(stream::iter(events)))
        }
    }

    fn img(url: &str) -> ContentPart {
        ContentPart::ImageUrl { url: url.into(), detail: ImageDetail::Auto }
    }

    #[tokio::test]
    async fn translate_part_aggregates_stream_and_writes_cache() {
        let calls = Arc::new(Mutex::new(0usize));
        let provider = MockProvider { reply: "a cat".into(), calls: Arc::clone(&calls) };
        let cache = LruDescriptionCache::new(8);
        let part = img("https://example.com/cat.jpg");

        let out = translate_part(&provider, "m", &part, &IMAGE_SPEC, &cache, "s1")
            .await
            .unwrap();
        assert_eq!(out, "a cat");
        assert_eq!(*calls.lock().unwrap(), 1);

        // Second call hits the cache; provider is not invoked again.
        let out2 = translate_part(&provider, "m", &part, &IMAGE_SPEC, &cache, "s1")
            .await
            .unwrap();
        assert_eq!(out2, "a cat");
        assert_eq!(*calls.lock().unwrap(), 1);

        // And the cache holds it under the fingerprint.
        let key = fingerprint(&part).unwrap();
        assert_eq!(cache.get("s1", &key).as_deref(), Some("a cat"));
        // A different session does not see it (sessions are isolated).
        assert_eq!(cache.get("s2", &key), None);
    }

    #[tokio::test]
    async fn adapt_last_turn_replaces_media_with_numbered_description() {
        let provider: Arc<dyn ChatProvider> = Arc::new(MockProvider {
            reply: "desc".into(),
            calls: Arc::new(Mutex::new(0)),
        });
        let cache = LruDescriptionCache::new(8);
        let mut msg = ChatMessage {
            role: "user".into(),
            parts: vec![
                img("https://a/1.jpg"),
                img("https://a/2.jpg"),
                ContentPart::Text { text: "what are these?".into() },
            ],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            is_error: None,
        };

        adapt_last_turn_media(
            &mut msg,
            &IMAGE_SPEC,
            Some((&provider, "m")),
            &cache,
            "s1",
        )
        .await;

        // Two images -> one numbered description part, user text preserved.
        assert_eq!(msg.parts.len(), 2);
        match &msg.parts[0] {
            ContentPart::Text { text } => {
                assert!(text.contains("[图片1描述]: desc"));
                assert!(text.contains("[图片2描述]: desc"));
            }
            other => panic!("expected text, got {other:?}"),
        }
        assert!(matches!(&msg.parts[1], ContentPart::Text { text } if text == "what are these?"));
    }

    #[tokio::test]
    async fn adapt_last_turn_no_aux_degrades_to_placeholder() {
        let cache = LruDescriptionCache::new(8);
        let mut msg = ChatMessage {
            role: "user".into(),
            parts: vec![img("https://a/1.jpg")],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            is_error: None,
        };

        adapt_last_turn_media(&mut msg, &IMAGE_SPEC, None, &cache, "s1").await;
        assert_eq!(msg.parts.len(), 1);
        assert!(matches!(&msg.parts[0], ContentPart::Text { text } if text.contains("no image model available")));
    }

    #[test]
    fn adapt_history_reuses_cache_else_placeholder() {
        let cache = LruDescriptionCache::new(8);
        let part = img("https://a/1.jpg");
        cache.put("s1", fingerprint(&part).unwrap(), "cached desc".into());

        let mut messages = vec![
            ChatMessage {
                role: "user".into(),
                parts: vec![part.clone()],
                name: None,
                tool_call_id: None,
                tool_calls: None,
                is_error: None,
            },
            ChatMessage {
                role: "user".into(),
                parts: vec![img("https://a/2.jpg")],
                name: None,
                tool_call_id: None,
                tool_calls: None,
                is_error: None,
            },
        ];

        adapt_history_media(&mut messages, &IMAGE_SPEC, &cache, "s1", None);

        assert!(matches!(&messages[0].parts[0], ContentPart::Text { text } if text == "[图片描述]: cached desc"));
        assert!(matches!(&messages[1].parts[0], ContentPart::Text { text } if text == "[image]"));
    }

    #[test]
    fn adapt_history_skips_skip_idx() {
        let cache = LruDescriptionCache::new(8);
        let mut messages = vec![ChatMessage {
            role: "user".into(),
            parts: vec![img("https://a/1.jpg")],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            is_error: None,
        }];
        adapt_history_media(&mut messages, &IMAGE_SPEC, &cache, "s1", Some(0));
        // Untouched: still the raw image part.
        assert!(matches!(&messages[0].parts[0], ContentPart::ImageUrl { .. }));
    }

    #[test]
    fn persistent_cache_survives_reopen() {
        // `dir` stands in for the sessions root; descriptions land under
        // `{root}/{session}/descriptions/`.
        let root = tempfile::tempdir().unwrap();
        let key = "deadbeef".to_string();

        // Write through the first instance, then drop it (simulates shutdown).
        {
            let cache = PersistentDescriptionCache::open(root.path(), 8);
            assert_eq!(cache.get("s1", &key), None);
            cache.put("s1", key.clone(), "a red bicycle".into());
            assert_eq!(cache.get("s1", &key).as_deref(), Some("a red bicycle"));
        }

        // The cold file lives in the session's own directory (sibling of blobs/).
        assert!(root
            .path()
            .join("s1")
            .join("descriptions")
            .join(format!("{key}.txt"))
            .exists());

        // A fresh instance with an empty hot tier still finds it on disk —
        // this is the across-restart recovery the in-memory LRU could not give.
        let reopened = PersistentDescriptionCache::open(root.path(), 8);
        assert_eq!(reopened.get("s1", &key).as_deref(), Some("a red bicycle"));
        // The read-through warmed the hot tier (second get also succeeds).
        assert_eq!(reopened.get("s1", &key).as_deref(), Some("a red bicycle"));
        // Another session is isolated — its cold dir was never written.
        assert_eq!(reopened.get("s2", &key), None);
    }

    #[test]
    fn persistent_cache_miss_is_none() {
        let root = tempfile::tempdir().unwrap();
        let cache = PersistentDescriptionCache::open(root.path(), 8);
        assert_eq!(cache.get("s1", "never-written"), None);
    }

    #[test]
    fn persistent_cache_drives_history_reuse_after_reopen() {
        // End-to-end: a description persisted on one run is reused by
        // `adapt_history_media` on the next, instead of degrading to "[image]".
        let root = tempfile::tempdir().unwrap();
        let part = img("https://a/photo.jpg");
        let key = fingerprint(&part).unwrap();
        {
            let cache = PersistentDescriptionCache::open(root.path(), 8);
            cache.put("s1", key, "a mountain lake".into());
        }
        let cache = PersistentDescriptionCache::open(root.path(), 8);
        let mut messages = vec![ChatMessage {
            role: "user".into(),
            parts: vec![part],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            is_error: None,
        }];
        adapt_history_media(&mut messages, &IMAGE_SPEC, &cache, "s1", None);
        match &messages[0].parts[0] {
            ContentPart::Text { text } => {
                assert!(text.contains("a mountain lake"), "got: {text}");
                assert!(!text.contains("[image]"), "should not degrade: {text}");
            }
            other => panic!("expected reused description text, got {other:?}"),
        }
    }

    #[test]
    fn fingerprint_invariant_across_externalization() {
        use crate::providers::capability_chat::sha256_hex;
        use base64::Engine;
        let bytes = vec![42u8; 256];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let hash = sha256_hex(&bytes);

        let inline = ContentPart::ImageB64 {
            b64_json: b64,
            media_type: None,
            detail: ImageDetail::Auto,
        };
        let externalized = ContentPart::ImageRef {
            hash: hash.clone(),
            media_type: None,
            detail: ImageDetail::Auto,
        };

        // The fingerprint is the decoded-bytes sha256 (== the blob hash), so it
        // is the same whether the part is inline or externalized — this is what
        // lets a cache key persisted before externalization still match, and the
        // description sweep recognize a live image in either form.
        assert_eq!(fingerprint(&inline).as_deref(), Some(hash.as_str()));
        assert_eq!(fingerprint(&externalized).as_deref(), Some(hash.as_str()));
        assert_eq!(fingerprint(&inline), fingerprint(&externalized));
    }
}
