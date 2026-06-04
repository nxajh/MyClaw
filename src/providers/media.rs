//! Per-model media lowering — the boundary between the *canonical* message
//! format (rich `ImageB64`/`AudioB64` parts in their real form, as the agent
//! emits them) and the *lowered* form a concrete model can actually accept.
//!
//! A model that lacks native input for a modality cannot receive those parts on
//! the wire (the protocol renderers `unreachable!` on them). Instead of the
//! agent guessing which model will serve and pre-rendering for it, lowering
//! happens **per concrete model**, right above each per-model provider: every
//! chat provider registered in the [`crate::registry`] is wrapped in a
//! [`MediaLoweringProvider`] carrying that model's [`MediaCaps`]. So the fallback
//! chain, a `/model` override, and aux tool-calls all lower identically and
//! automatically, and the agent always sends one canonical format.
//!
//! Lowering replaces an unsupported part with a neutral marker — `[图片 #N]` /
//! `[语音 #N]` — numbered by scan order. The marker is deliberately tool-agnostic:
//! the `view_image` / `hear_audio` tools (which live in the agent loop, because
//! retrieval is a multi-round, model-calling concern) describe how to resolve a
//! marker by its index. The numbering here and the index resolution there share
//! this module's scan-order convention.

use async_trait::async_trait;
use std::sync::Arc;

use crate::providers::capability_chat::{
    BoxStream, ChatMessage, ChatProvider, ChatRequest, ContentPart, StreamEvent,
};

/// The canonical marker for the `n`-th image (1-based, by scan order). Shared by
/// the lowering pass (which emits it) and the `view_image` tool (which resolves
/// the `n`-th image from history).
pub fn image_marker(n: usize) -> String {
    format!("[图片 #{n}]")
}

/// The canonical marker for the `n`-th audio clip (1-based, by scan order).
/// Shared by the lowering pass and the `hear_audio` tool.
pub fn audio_marker(n: usize) -> String {
    format!("[语音 #{n}]")
}

/// What input modalities a concrete model accepts natively. Anything not set is
/// lowered to a marker for that model.
#[derive(Debug, Clone, Copy)]
pub struct MediaCaps {
    pub image: bool,
    pub audio: bool,
}

fn is_image(p: &ContentPart) -> bool {
    matches!(p, ContentPart::ImageB64 { .. } | ContentPart::ImageUrl { .. })
}
fn is_audio(p: &ContentPart) -> bool {
    matches!(p, ContentPart::AudioB64 { .. })
}

/// Lower every media part the model can't take natively to its marker, leaving
/// supported parts untouched. Returns `None` when nothing changes — so the
/// common native / text-only paths avoid cloning the (often large) message list.
///
/// Image and audio are numbered with independent 1-based counters, matching how
/// `view_image` / `hear_audio` resolve the `n`-th part of each kind.
pub fn lower_media_for(messages: &[ChatMessage], caps: MediaCaps) -> Option<Vec<ChatMessage>> {
    let needs = messages.iter().any(|m| {
        m.parts
            .iter()
            .any(|p| (is_image(p) && !caps.image) || (is_audio(p) && !caps.audio))
    });
    if !needs {
        return None;
    }

    let mut img_n = 0usize;
    let mut aud_n = 0usize;
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        if !m.parts.iter().any(|p| is_image(p) || is_audio(p)) {
            out.push(m.clone());
            continue;
        }
        let mut nm = m.clone();
        let mut new_parts = Vec::with_capacity(nm.parts.len());
        for part in std::mem::take(&mut nm.parts) {
            match part {
                ContentPart::ImageB64 { .. } | ContentPart::ImageUrl { .. } => {
                    img_n += 1;
                    if caps.image {
                        new_parts.push(part);
                    } else {
                        new_parts.push(ContentPart::Text { text: image_marker(img_n) });
                    }
                }
                ContentPart::AudioB64 { .. } => {
                    aud_n += 1;
                    if caps.audio {
                        new_parts.push(part);
                    } else {
                        new_parts.push(ContentPart::Text { text: audio_marker(aud_n) });
                    }
                }
                other => new_parts.push(other),
            }
        }
        nm.parts = new_parts;
        out.push(nm);
    }
    Some(out)
}

/// Wraps a single concrete model's chat provider and lowers any media that model
/// can't take natively before delegating. One per (model, provider) registration
/// — see [`crate::registry::Registry::register_chat`].
pub struct MediaLoweringProvider {
    inner: Arc<dyn ChatProvider>,
    caps: MediaCaps,
}

impl MediaLoweringProvider {
    pub fn new(inner: Arc<dyn ChatProvider>, caps: MediaCaps) -> Self {
        Self { inner, caps }
    }
}

#[async_trait]
impl ChatProvider for MediaLoweringProvider {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        match lower_media_for(req.messages, self.caps) {
            // Nothing to lower — pass the borrowed request straight through.
            None => self.inner.chat(req),
            Some(lowered) => {
                let req = ChatRequest { messages: &lowered, ..req };
                self.inner.chat(req)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::capability_chat::ImageDetail;

    fn img(b: &str) -> ContentPart {
        ContentPart::ImageB64 { b64_json: b.into(), media_type: None, detail: ImageDetail::Auto }
    }
    fn aud(b: &str) -> ContentPart {
        ContentPart::AudioB64 { b64_json: b.into(), media_type: None }
    }
    fn msg(parts: Vec<ContentPart>) -> ChatMessage {
        ChatMessage {
            role: "user".into(),
            parts,
            name: None,
            tool_call_id: None,
            tool_calls: None,
            is_error: None,
        }
    }
    fn texts(ms: &[ChatMessage]) -> Vec<String> {
        ms.iter()
            .flat_map(|m| &m.parts)
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn full_native_is_noop() {
        let ms = vec![msg(vec![img("A"), aud("B")])];
        let caps = MediaCaps { image: true, audio: true };
        assert!(lower_media_for(&ms, caps).is_none(), "no clone when everything is supported");
    }

    #[test]
    fn text_only_model_markers_both_with_independent_counters() {
        let ms = vec![
            msg(vec![ContentPart::Text { text: "hi".into() }, img("A")]),
            msg(vec![aud("B"), img("C")]),
        ];
        let caps = MediaCaps { image: false, audio: false };
        let out = lower_media_for(&ms, caps).expect("should lower");
        assert!(!out.iter().flat_map(|m| &m.parts).any(|p| is_image(p) || is_audio(p)));
        let t = texts(&out);
        assert!(t.contains(&"[图片 #1]".to_string()));
        assert!(t.contains(&"[图片 #2]".to_string()));
        assert!(t.contains(&"[语音 #1]".to_string()));
    }

    #[test]
    fn partial_caps_lowers_only_unsupported_modality() {
        // Supports images, not audio: image stays native, audio → marker.
        let ms = vec![msg(vec![img("A"), aud("B")])];
        let caps = MediaCaps { image: true, audio: false };
        let out = lower_media_for(&ms, caps).expect("audio needs lowering");
        assert_eq!(out[0].parts.iter().filter(|p| is_image(p)).count(), 1, "image kept native");
        assert!(!out[0].parts.iter().any(|p| is_audio(p)), "audio lowered");
        assert!(texts(&out).contains(&"[语音 #1]".to_string()));
    }
}
