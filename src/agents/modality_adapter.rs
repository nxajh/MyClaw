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
use sha2::{Digest, Sha256};
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

/// Content fingerprint used as the description-cache key.
/// URL images key on the URL; base64 images key on the payload bytes.
/// Returns `None` for non-media parts (which have no stable identity to cache).
pub fn fingerprint(part: &ContentPart) -> Option<String> {
    let seed = match part {
        ContentPart::ImageUrl { url, .. } => url.as_str(),
        ContentPart::ImageB64 { b64_json, .. } => b64_json.as_str(),
        _ => return None,
    };
    Some(format!("{:x}", Sha256::digest(seed.as_bytes())))
}

// ── Description cache (T2.2) ───────────────────────────────────────────────────

/// Process-wide cache: content fingerprint → text description.
/// Shared via the runtime so it survives across turns and sessions.
pub trait DescriptionCache: Send + Sync {
    /// Look up a cached description by fingerprint key.
    fn get(&self, key: &str) -> Option<String>;
    /// Store a description for a fingerprint key.
    fn put(&self, key: String, value: String);
}

/// LRU-backed [`DescriptionCache`]. The cache key is a content sha256, so the
/// description is content-addressed and never needs explicit invalidation
/// (the same media content always yields the same description).
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
    fn get(&self, key: &str) -> Option<String> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.get(key).cloned()
    }

    fn put(&self, key: String, value: String) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.put(key, value);
    }
}

/// Two-tier [`DescriptionCache`]: a bounded in-memory LRU hot tier backed by a
/// content-addressed on-disk cold tier under `dir`. The key is the same content
/// fingerprint as [`LruDescriptionCache`], so descriptions survive process
/// restarts and hot-tier eviction — a non-vision model recovers historical-image
/// descriptions without re-invoking the auxiliary model, mirroring how the image
/// bytes themselves are persisted as content-addressed blobs (`storage::json_file`).
///
/// The cold tier is intentionally unbounded: each entry is a few KB of text and
/// content-addressed (identical media never duplicates), so growth is slow. A
/// future pass can sweep entries with no live blob; the store is correct without it.
pub struct PersistentDescriptionCache {
    hot: Mutex<lru::LruCache<String, String>>,
    dir: PathBuf,
}

impl PersistentDescriptionCache {
    /// Open (or create) the cold-tier directory at `dir`, with a `capacity`-entry
    /// in-memory hot tier (clamped to >= 1). A directory-creation failure is
    /// logged and tolerated — the cache then behaves as a memory-only LRU.
    pub fn open(dir: impl Into<PathBuf>, capacity: usize) -> Self {
        let dir = dir.into();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(
                path = %dir.display(), err = %e,
                "failed to create description cache dir; running memory-only"
            );
        }
        let cap = NonZeroUsize::new(capacity.max(1)).expect("capacity >= 1");
        Self {
            hot: Mutex::new(lru::LruCache::new(cap)),
            dir,
        }
    }

    /// Cold-tier file path for `key`. The key is a sha256 hex string, so it is
    /// always a safe single-segment filename.
    fn path_for(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.txt"))
    }
}

impl DescriptionCache for PersistentDescriptionCache {
    fn get(&self, key: &str) -> Option<String> {
        {
            let mut guard = self.hot.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(hit) = guard.get(key) {
                return Some(hit.clone());
            }
        }
        // Cold-tier read-through: a hit warms the hot tier for next time.
        let text = std::fs::read_to_string(self.path_for(key)).ok()?;
        let mut guard = self.hot.lock().unwrap_or_else(|e| e.into_inner());
        guard.put(key.to_string(), text.clone());
        Some(text)
    }

    fn put(&self, key: String, value: String) {
        {
            let mut guard = self.hot.lock().unwrap_or_else(|e| e.into_inner());
            guard.put(key.clone(), value.clone());
        }
        // Write-through, atomic (temp + rename) so a partial write can never be
        // read back as a truncated description.
        let path = self.path_for(&key);
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
) -> anyhow::Result<String> {
    if let Some(key) = fingerprint(part) {
        if let Some(hit) = cache.get(&key) {
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
        cache.put(key, text.clone());
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
                .and_then(|k| cache.get(&k))
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
                .map(|part| translate_part(provider.as_ref(), model_id, part, spec, cache));
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

        let out = translate_part(&provider, "m", &part, &IMAGE_SPEC, &cache)
            .await
            .unwrap();
        assert_eq!(out, "a cat");
        assert_eq!(*calls.lock().unwrap(), 1);

        // Second call hits the cache; provider is not invoked again.
        let out2 = translate_part(&provider, "m", &part, &IMAGE_SPEC, &cache)
            .await
            .unwrap();
        assert_eq!(out2, "a cat");
        assert_eq!(*calls.lock().unwrap(), 1);

        // And the cache holds it under the fingerprint.
        let key = fingerprint(&part).unwrap();
        assert_eq!(cache.get(&key).as_deref(), Some("a cat"));
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

        adapt_last_turn_media(&mut msg, &IMAGE_SPEC, None, &cache).await;
        assert_eq!(msg.parts.len(), 1);
        assert!(matches!(&msg.parts[0], ContentPart::Text { text } if text.contains("no image model available")));
    }

    #[test]
    fn adapt_history_reuses_cache_else_placeholder() {
        let cache = LruDescriptionCache::new(8);
        let part = img("https://a/1.jpg");
        cache.put(fingerprint(&part).unwrap(), "cached desc".into());

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

        adapt_history_media(&mut messages, &IMAGE_SPEC, &cache, None);

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
        adapt_history_media(&mut messages, &IMAGE_SPEC, &cache, Some(0));
        // Untouched: still the raw image part.
        assert!(matches!(&messages[0].parts[0], ContentPart::ImageUrl { .. }));
    }

    #[test]
    fn persistent_cache_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let key = "deadbeef".to_string();

        // Write through the first instance, then drop it (simulates shutdown).
        {
            let cache = PersistentDescriptionCache::open(dir.path(), 8);
            assert_eq!(cache.get(&key), None);
            cache.put(key.clone(), "a red bicycle".into());
            assert_eq!(cache.get(&key).as_deref(), Some("a red bicycle"));
        }

        // A fresh instance with an empty hot tier still finds it on disk —
        // this is the across-restart recovery the in-memory LRU could not give.
        let reopened = PersistentDescriptionCache::open(dir.path(), 8);
        assert_eq!(reopened.get(&key).as_deref(), Some("a red bicycle"));
        // The read-through warmed the hot tier (second get also succeeds).
        assert_eq!(reopened.get(&key).as_deref(), Some("a red bicycle"));
    }

    #[test]
    fn persistent_cache_miss_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let cache = PersistentDescriptionCache::open(dir.path(), 8);
        assert_eq!(cache.get("never-written"), None);
    }

    #[test]
    fn persistent_cache_drives_history_reuse_after_reopen() {
        // End-to-end: a description persisted on one run is reused by
        // `adapt_history_media` on the next, instead of degrading to "[image]".
        let dir = tempfile::tempdir().unwrap();
        let part = img("https://a/photo.jpg");
        let key = fingerprint(&part).unwrap();
        {
            let cache = PersistentDescriptionCache::open(dir.path(), 8);
            cache.put(key, "a mountain lake".into());
        }
        let cache = PersistentDescriptionCache::open(dir.path(), 8);
        let mut messages = vec![ChatMessage {
            role: "user".into(),
            parts: vec![part],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            is_error: None,
        }];
        adapt_history_media(&mut messages, &IMAGE_SPEC, &cache, None);
        match &messages[0].parts[0] {
            ContentPart::Text { text } => {
                assert!(text.contains("a mountain lake"), "got: {text}");
                assert!(!text.contains("[image]"), "should not degrade: {text}");
            }
            other => panic!("expected reused description text, got {other:?}"),
        }
    }
}
