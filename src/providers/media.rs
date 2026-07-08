//! Per-model media lowering — converts canonical rich/file content into the
//! concrete form a model can accept, or into path markers for unsupported media.

use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;

use crate::providers::capability_chat::{
    BoxStream, ChatMessage, ChatProvider, ChatRequest, ContentPart, StreamEvent,
};
use crate::providers::provider_id::{ProviderId, well_known};

pub fn image_marker_path(path: &str) -> String {
    format!("[图片: {path}]")
}

pub fn audio_marker_path(path: &str) -> String {
    format!("[语音: {path}]")
}

pub fn video_marker_path(path: &str) -> String {
    format!("[视频: {path}]")
}

pub fn file_marker_path(path: &str) -> String {
    format!("[文件: {path}]")
}

/// Legacy marker for old in-memory/base64 history.
pub fn image_marker(n: usize) -> String {
    format!("[图片 #{n}]")
}

/// Legacy marker for old in-memory/base64 history.
pub fn audio_marker(n: usize) -> String {
    format!("[语音 #{n}]")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileModality {
    Image,
    Audio,
    Video,
    Other,
}

pub fn modality_from_mime(mime: Option<&str>, path: &str) -> FileModality {
    if let Some(mime) = mime.map(|m| m.split(';').next().unwrap_or(m).trim().to_ascii_lowercase()) {
        if mime.starts_with("image/") {
            return FileModality::Image;
        }
        if mime.starts_with("audio/") {
            return FileModality::Audio;
        }
        if mime.starts_with("video/") {
            return FileModality::Video;
        }
    }
    match Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" => FileModality::Image,
        "ogg" | "mp3" | "wav" | "flac" | "m4a" | "aac" => FileModality::Audio,
        "mp4" | "webm" | "mov" | "mkv" => FileModality::Video,
        _ => sniff_modality_from_path(path),
    }
}

/// Last-resort modality detection by reading file magic bytes.
/// Only called when both mime_type and file extension are unavailable.
fn sniff_modality_from_path(path: &str) -> FileModality {
    let abs = resolve_path(path);
    let Ok(bytes) = std::fs::read(&abs) else {
        return FileModality::Other;
    };
    sniff_modality_from_bytes(&bytes)
}

/// Detect file modality from magic bytes (file signature).
fn sniff_modality_from_bytes(bytes: &[u8]) -> FileModality {
    if bytes.len() < 4 {
        return FileModality::Other;
    }
    // Image magic bytes
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return FileModality::Image; // JPEG
    }
    if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        return FileModality::Image; // PNG
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return FileModality::Image; // GIF
    }
    if bytes.starts_with(b"RIFF") && bytes.len() >= 12 && &bytes[8..12] == b"WEBP" {
        return FileModality::Image; // WebP
    }
    if bytes.starts_with(&[0x42, 0x4D]) {
        return FileModality::Image; // BMP
    }
    // Audio magic bytes
    if bytes.starts_with(b"ID3")
        || (bytes.len() >= 2 && bytes[0] == 0xFF && (bytes[1] & 0xE0) == 0xE0)
    {
        return FileModality::Audio; // MP3
    }
    if bytes.starts_with(b"OggS") {
        return FileModality::Audio; // OGG
    }
    if bytes.starts_with(b"RIFF") && bytes.len() >= 12 && &bytes[8..12] == b"WAVE" {
        return FileModality::Audio; // WAV
    }
    if bytes.starts_with(b"fLaC") {
        return FileModality::Audio; // FLAC
    }
    // Video magic bytes
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        return FileModality::Video; // MP4/MOV (ISO BMFF)
    }
    if bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return FileModality::Video; // WebM/MKV (EBML)
    }
    FileModality::Other
}

pub fn marker_for_file(path: &str, mime: Option<&str>) -> String {
    match modality_from_mime(mime, path) {
        FileModality::Image => image_marker_path(path),
        FileModality::Audio => audio_marker_path(path),
        FileModality::Video => video_marker_path(path),
        FileModality::Other => file_marker_path(path),
    }
}

/// Age media in a message: replace all `File` parts with compact text markers.
///
/// Called after a turn completes so that subsequent turns don't re-upload
/// potentially large media payloads the model has already seen. The marker
/// preserves the file path so the agent can still reference it (e.g. via
/// `view_video` / `view_image` / `hear_audio` tools) if the user asks again.
///
/// Returns `true` if any part was changed.
pub fn age_media_in_message(msg: &mut ChatMessage) -> bool {
    let mut changed = false;
    for part in &mut msg.parts {
        if let ContentPart::File {
            path, mime_type, ..
        } = part
        {
            let marker = marker_for_file(path, mime_type.as_deref());
            *part = ContentPart::Text { text: marker };
            changed = true;
        }
    }
    changed
}

/// Infer MIME type from file name extension. Returns `None` if unknown.
pub fn infer_mime_from_name(file_name: &str) -> Option<String> {
    let ext = file_name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        "svg" => Some("image/svg+xml"),
        "mp4" => Some("video/mp4"),
        "webm" => Some("video/webm"),
        "mov" => Some("video/quicktime"),
        "mkv" => Some("video/x-matroska"),
        "mp3" => Some("audio/mpeg"),
        "wav" => Some("audio/wav"),
        "ogg" => Some("audio/ogg"),
        "flac" => Some("audio/flac"),
        "m4a" => Some("audio/mp4"),
        "aac" => Some("audio/aac"),
        "pdf" => Some("application/pdf"),
        "zip" => Some("application/zip"),
        "txt" => Some("text/plain"),
        "json" => Some("application/json"),
        "csv" => Some("text/csv"),
        "html" | "htm" => Some("text/html"),
        "xml" => Some("application/xml"),
        _ => None,
    }
    .map(|s| s.to_string())
}

/// Infer MIME type from the first bytes of a file (magic bytes).
/// Reads at most 512 bytes. Returns `None` if unrecognised.
pub async fn infer_mime_from_file_head(path: &Path) -> Option<String> {
    const HEAD_LEN: usize = 512;
    let mut buf = vec![0u8; HEAD_LEN];
    let n = match tokio::fs::File::open(path).await {
        Ok(mut f) => {
            use tokio::io::AsyncReadExt;
            f.read(&mut buf).await.unwrap_or(0)
        }
        Err(_) => return None,
    };
    let data = &buf[..n];

    if data.len() >= 4 && &data[0..4] == b"\x89PNG" {
        return Some("image/png".to_string());
    }
    if data.len() >= 2 && &data[0..2] == b"\xff\xd8" {
        return Some("image/jpeg".to_string());
    }
    if data.len() >= 4 && &data[0..4] == b"GIF8" {
        return Some("image/gif".to_string());
    }
    if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WAVE" {
        return Some("audio/wav".to_string());
    }
    if data.len() >= 3 && &data[0..3] == b"ID3" {
        return Some("audio/mpeg".to_string());
    }
    if data.len() >= 5 && &data[0..5] == b"%PDF-" {
        return Some("application/pdf".to_string());
    }
    None
}

/// Combined inference: name extension first, then magic bytes, then
/// `application/octet-stream` as final fallback.
pub async fn infer_mime(file_name: &str, path: &Path) -> String {
    if let Some(mime) = infer_mime_from_name(file_name) {
        return mime;
    }
    if let Some(mime) = infer_mime_from_file_head(path).await {
        return mime;
    }
    "application/octet-stream".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaTransport {
    Marker,
    InlineBase64,
}

#[derive(Debug, Clone, Copy)]
pub struct MediaInputPolicy {
    pub model_supports: bool,
    pub transport: MediaTransport,
    pub max_inline_bytes: Option<u64>,
}

impl MediaInputPolicy {
    pub fn marker(model_supports: bool) -> Self {
        Self {
            model_supports,
            transport: MediaTransport::Marker,
            max_inline_bytes: None,
        }
    }

    pub fn inline_base64(model_supports: bool, max_inline_bytes: Option<u64>) -> Self {
        Self {
            model_supports,
            transport: MediaTransport::InlineBase64,
            max_inline_bytes,
        }
    }

    fn can_inline(&self, size_bytes: Option<u64>) -> bool {
        if !self.model_supports || self.transport != MediaTransport::InlineBase64 {
            return false;
        }
        match (self.max_inline_bytes, size_bytes) {
            (Some(max), Some(size)) => size <= max,
            _ => true,
        }
    }
}

/// Provider/protocol media policy for file lowering.
///
/// `model_config.input` only says the model understands a modality; this policy
/// says the current provider/protocol renderer can safely carry it. Unsupported
/// or over-limit media becomes a path marker.
#[derive(Debug, Clone, Copy)]
pub struct MediaPolicy {
    pub image: MediaInputPolicy,
    pub audio: MediaInputPolicy,
    pub video: MediaInputPolicy,
    pub other: MediaInputPolicy,
}

impl MediaPolicy {
    pub fn from_model_support(image: bool, audio: bool, video: bool) -> Self {
        Self {
            image: MediaInputPolicy::inline_base64(image, Some(25 * 1024 * 1024)),
            audio: MediaInputPolicy::marker(audio),
            video: MediaInputPolicy::marker(video),
            other: MediaInputPolicy::marker(false),
        }
    }

    pub fn for_provider_protocol_model(
        provider_id: &ProviderId,
        protocol: crate::config::provider::Protocol,
        model_config: &crate::providers::capability::ChatModelConfig,
    ) -> Self {
        use crate::providers::capability::Modality;

        let image = model_config.supports_input(Modality::Image);
        let audio = model_config.supports_input(Modality::Audio);
        let video = model_config.supports_input(Modality::Video);

        let image_policy = match provider_id.as_str() {
            well_known::OPENAI
            | well_known::ANTHROPIC
            | well_known::GLM
            | well_known::GENERIC
            | well_known::GOOGLE => MediaInputPolicy::inline_base64(image, Some(25 * 1024 * 1024)),
            well_known::XIAOMI | well_known::MINIMAX => {
                // Both Anthropic and OpenAI renderers handle inline images.
                MediaInputPolicy::inline_base64(image, Some(25 * 1024 * 1024))
            }
            _ => MediaInputPolicy::marker(image),
        };

        let audio_policy = match provider_id.as_str() {
            well_known::OPENAI | well_known::GOOGLE => {
                MediaInputPolicy::inline_base64(audio, Some(25 * 1024 * 1024))
            }
            // Xiaomi OpenAI protocol supports input_audio natively when the
            // model declares audio support. Models without audio support get
            // a marker so the agent can delegate via hear_audio tool.
            well_known::XIAOMI if protocol == crate::config::provider::Protocol::OpenAi => {
                MediaInputPolicy::inline_base64(audio, Some(25 * 1024 * 1024))
            }
            // Xiaomi Anthropic protocol: audio/video not supported by the
            // Anthropic Messages wire format; fall back to text markers.
            _ => MediaInputPolicy::marker(audio),
        };

        let video_policy = match provider_id.as_str() {
            well_known::GLM | well_known::GENERIC | well_known::GOOGLE => {
                MediaInputPolicy::inline_base64(video, Some(50 * 1024 * 1024))
            }
            // Xiaomi OpenAI protocol supports video_url natively when the
            // model declares video support. Models without video support get
            // a marker so the agent can delegate via view_video tool.
            well_known::XIAOMI if protocol == crate::config::provider::Protocol::OpenAi => {
                MediaInputPolicy::inline_base64(video, Some(50 * 1024 * 1024))
            }
            _ => MediaInputPolicy::marker(video),
        };

        Self {
            image: image_policy,
            audio: audio_policy,
            video: video_policy,
            other: MediaInputPolicy::marker(false),
        }
    }

    pub fn for_provider_model(
        provider_id: &ProviderId,
        model_config: &crate::providers::capability::ChatModelConfig,
    ) -> Self {
        Self::for_provider_protocol_model(
            provider_id,
            crate::config::provider::Protocol::OpenAi,
            model_config,
        )
    }
}

/// Backward-compatible alias for older call sites/tests. Prefer `MediaPolicy`.
pub type MediaCaps = MediaPolicy;

fn is_file(p: &ContentPart) -> bool {
    matches!(p, ContentPart::File { .. })
}

pub fn resolve_path(path: &str) -> std::path::PathBuf {
    let p = std::path::PathBuf::from(path);
    if p.is_absolute() {
        p
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(p)
    }
}

fn lower_file_part(
    path: String,
    mime_type: Option<String>,
    name: Option<String>,
    size_bytes: Option<u64>,
    policy: MediaPolicy,
) -> ContentPart {
    let modality = modality_from_mime(mime_type.as_deref(), &path);
    let supported = match modality {
        FileModality::Image => policy.image.can_inline(size_bytes),
        FileModality::Audio => policy.audio.can_inline(size_bytes),
        FileModality::Video => policy.video.can_inline(size_bytes),
        FileModality::Other => policy.other.can_inline(size_bytes),
    };
    if supported {
        tracing::debug!(
            path = %path,
            modality = ?modality,
            size_bytes,
            "lower_file_part: keeping inline"
        );
        ContentPart::File {
            path,
            mime_type,
            name,
            size_bytes,
        }
    } else {
        let marker = marker_for_file(&path, mime_type.as_deref());
        tracing::info!(
            path = %path,
            modality = ?modality,
            size_bytes,
            marker = %marker,
            "lower_file_part: converting to text marker (exceeds policy or unsupported)"
        );
        ContentPart::Text { text: marker }
    }
}

pub fn infer_image_mime(path: &str) -> Option<&'static str> {
    match Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

pub fn infer_audio_mime(path: &str) -> Option<&'static str> {
    match Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "ogg" => Some("audio/ogg"),
        "mp3" => Some("audio/mpeg"),
        "wav" => Some("audio/wav"),
        "flac" => Some("audio/flac"),
        "m4a" => Some("audio/m4a"),
        _ => None,
    }
}

pub fn infer_video_mime(path: &str) -> Option<&'static str> {
    match Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "mp4" => Some("video/mp4"),
        "webm" => Some("video/webm"),
        "mov" => Some("video/quicktime"),
        "mkv" => Some("video/x-matroska"),
        _ => None,
    }
}

pub fn lower_media_for(messages: &[ChatMessage], policy: MediaPolicy) -> Option<Vec<ChatMessage>> {
    let needs = messages.iter().any(|m| m.parts.iter().any(is_file));
    if !needs {
        return None;
    }

    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        if !m.parts.iter().any(is_file) {
            out.push(m.clone());
            continue;
        }
        let mut nm = m.clone();
        let mut new_parts = Vec::with_capacity(nm.parts.len());
        for part in std::mem::take(&mut nm.parts) {
            match part {
                ContentPart::File {
                    path,
                    mime_type,
                    name,
                    size_bytes,
                } => {
                    new_parts.push(lower_file_part(path, mime_type, name, size_bytes, policy));
                }
                other => new_parts.push(other),
            }
        }
        nm.parts = new_parts;
        out.push(nm);
    }
    Some(out)
}

pub struct MediaLoweringProvider {
    inner: Arc<dyn ChatProvider>,
    policy: MediaPolicy,
}

impl MediaLoweringProvider {
    pub fn new(inner: Arc<dyn ChatProvider>, policy: MediaPolicy) -> Self {
        Self { inner, policy }
    }
}

#[async_trait]
impl ChatProvider for MediaLoweringProvider {
    fn chat(&self, req: ChatRequest<'_>) -> anyhow::Result<BoxStream<StreamEvent>> {
        match lower_media_for(req.messages, self.policy) {
            None => self.inner.chat(req),
            Some(lowered) => {
                let req = ChatRequest {
                    messages: &lowered,
                    ..req
                };
                self.inner.chat(req)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_msg(parts: Vec<ContentPart>) -> ChatMessage {
        ChatMessage {
            role: "user".into(),
            parts,
            name: None,
            tool_call_id: None,
            tool_calls: None,
            is_error: None,
            model: None,
            usage: None,
        }
    }

    #[test]
    fn file_markers_use_path_and_modality() {
        assert_eq!(
            marker_for_file("sessions/s/files/photo.jpg", Some("image/jpeg")),
            "[图片: sessions/s/files/photo.jpg]"
        );
        assert_eq!(
            marker_for_file("sessions/s/files/voice.ogg", Some("audio/ogg; codecs=opus")),
            "[语音: sessions/s/files/voice.ogg]"
        );
        assert_eq!(
            marker_for_file("sessions/s/files/clip.mp4", None),
            "[视频: sessions/s/files/clip.mp4]"
        );
        assert_eq!(
            marker_for_file("sessions/s/files/report.pdf", Some("application/pdf")),
            "[文件: sessions/s/files/report.pdf]"
        );
    }

    #[test]
    fn lower_file_preserves_supported_image_for_renderer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.png");
        std::fs::write(&path, b"png-bytes").unwrap();
        let msg = user_msg(vec![ContentPart::File {
            path: path.to_string_lossy().to_string(),
            mime_type: Some("image/png".into()),
            name: Some("image.png".into()),
            size_bytes: Some(9),
        }]);

        let lowered =
            lower_media_for(&[msg], MediaPolicy::from_model_support(true, true, true)).unwrap();

        match &lowered[0].parts[0] {
            ContentPart::File {
                path: p, mime_type, ..
            } => {
                assert_eq!(p, &path.to_string_lossy().to_string());
                assert_eq!(mime_type.as_deref(), Some("image/png"));
            }
            other => panic!("expected File, got {other:?}"),
        }
    }

    #[test]
    fn lower_file_keeps_audio_video_as_markers_by_default() {
        let msg = user_msg(vec![
            ContentPart::File {
                path: "sessions/s/files/voice.ogg".into(),
                mime_type: Some("audio/ogg".into()),
                name: None,
                size_bytes: None,
            },
            ContentPart::File {
                path: "sessions/s/files/clip.mp4".into(),
                mime_type: Some("video/mp4".into()),
                name: None,
                size_bytes: None,
            },
        ]);

        let lowered =
            lower_media_for(&[msg], MediaPolicy::from_model_support(true, true, true)).unwrap();

        assert!(
            matches!(&lowered[0].parts[0], ContentPart::Text { text } if text == "[语音: sessions/s/files/voice.ogg]")
        );
        assert!(
            matches!(&lowered[0].parts[1], ContentPart::Text { text } if text == "[视频: sessions/s/files/clip.mp4]")
        );
    }

    #[test]
    fn openai_policy_can_preserve_audio_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voice.ogg");
        std::fs::write(&path, b"ogg-bytes").unwrap();
        let msg = user_msg(vec![ContentPart::File {
            path: path.to_string_lossy().to_string(),
            mime_type: Some("audio/ogg".into()),
            name: Some("voice.ogg".into()),
            size_bytes: Some(9),
        }]);
        let policy = MediaPolicy {
            image: MediaInputPolicy::marker(false),
            audio: MediaInputPolicy::inline_base64(true, Some(25 * 1024 * 1024)),
            video: MediaInputPolicy::marker(false),
            other: MediaInputPolicy::marker(false),
        };

        let lowered = lower_media_for(&[msg], policy).unwrap();
        match &lowered[0].parts[0] {
            ContentPart::File {
                path: p, mime_type, ..
            } => {
                assert_eq!(p, &path.to_string_lossy().to_string());
                assert_eq!(mime_type.as_deref(), Some("audio/ogg"));
            }
            other => panic!("expected File, got {other:?}"),
        }
    }

    #[test]
    fn sniff_detects_image_magic_bytes() {
        assert_eq!(
            sniff_modality_from_bytes(&[0xFF, 0xD8, 0xFF, 0xE0]),
            FileModality::Image
        ); // JPEG
        assert_eq!(
            sniff_modality_from_bytes(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A]),
            FileModality::Image // PNG
        );
    }

    #[test]
    fn sniff_detects_video_magic_bytes() {
        // MP4 ftyp box
        let mp4_header = [
            0x00, 0x00, 0x00, 0x20, b'f', b't', b'y', b'p', b'i', b's', b'o', b'm',
        ];
        assert_eq!(sniff_modality_from_bytes(&mp4_header), FileModality::Video);
        // WebM/MKV EBML
        assert_eq!(
            sniff_modality_from_bytes(&[0x1A, 0x45, 0xDF, 0xA3, 0x01, 0x00]),
            FileModality::Video
        );
    }

    #[test]
    fn sniff_detects_audio_magic_bytes() {
        assert_eq!(
            sniff_modality_from_bytes(b"ID3\x03\x00"),
            FileModality::Audio
        ); // MP3
        assert_eq!(sniff_modality_from_bytes(b"OggS\x00"), FileModality::Audio); // OGG
    }

    #[test]
    fn sniff_returns_other_for_unknown() {
        assert_eq!(sniff_modality_from_bytes(b"XXXX"), FileModality::Other);
        assert_eq!(
            sniff_modality_from_bytes(&[0x01, 0x02]),
            FileModality::Other
        );
    }
}
