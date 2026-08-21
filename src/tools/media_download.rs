//! Shared helper for multimedia tools: resolve a path that might be a URL.
//!
//! If the input starts with `http://` or `https://`, download the file to a
//! temp directory and return the local path. Otherwise, resolve the relative
//! path against the workspace cwd — except session-media marker paths
//! (`sessions/<id>/files/...`, as emitted by `[image: ...]` / `[video: ...]` /
//! `[audio: ...]` markers), which live under the data dir and are resolved
//! there instead (see `write_session_file`, which writes under
//! `AppConfig::sessions_root()` = `{base_dir}/sessions`).

use std::path::{Path, PathBuf};

/// Check if `s` looks like a URL.
pub fn is_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// True if `input` has the exact `sessions/<id>/files/<name>` shape produced
/// by `write_session_file` / the `[image: ...]` family of markers. Requires
/// a non-empty id segment and a literal `files` segment rather than just a
/// `sessions/` prefix, so an unrelated workspace directory that happens to
/// be named `sessions/` (e.g. a PHP/Express session store) isn't hijacked
/// into resolving against the data dir instead of cwd.
fn is_session_media_path(input: &str) -> bool {
    let mut segments = input.replace('\\', "/");
    segments = segments.trim_start_matches("./").to_string();
    let mut parts = segments.split('/');
    parts.next() == Some("sessions")
        && parts.next().is_some_and(|id| !id.is_empty())
        && parts.next() == Some("files")
        && parts.next().is_some_and(|name| !name.is_empty())
}

/// Resolve a path-or-URL to a local file path.
///
/// For URLs: downloads the content to a temp file and returns the path.
/// For local paths: absolute paths are used as-is; `sessions/<id>/files/...`
/// marker paths resolve against `data_dir`; other relative paths resolve
/// against cwd (workspace-relative).
pub async fn resolve_path_or_url(input: &str, data_dir: &Path) -> anyhow::Result<PathBuf> {
    if is_url(input) {
        download_to_temp(input).await
    } else {
        let p = PathBuf::from(input);
        if p.is_absolute() {
            Ok(p)
        } else if is_session_media_path(input) {
            Ok(data_dir.join(p))
        } else {
            Ok(std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(p))
        }
    }
}

/// Download a URL to a temp file and return its path.
async fn download_to_temp(url: &str) -> anyhow::Result<PathBuf> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .unwrap_or_default();

    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("failed to fetch URL '{}': {}", url, e))?;

    if !resp.status().is_success() {
        anyhow::bail!("HTTP {} when fetching '{}'", resp.status(), url);
    }

    // Infer extension from content-type or URL path.
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let ext = infer_extension_from_url(url, &content_type);

    let body = resp
        .bytes()
        .await
        .map_err(|e| anyhow::anyhow!("failed to read response from '{}': {}", url, e))?;

    // Write to a temp file.
    let tmp_dir = std::env::temp_dir().join("myclaw_downloads");
    std::fs::create_dir_all(&tmp_dir)?;

    let filename = format!("url_{}.{ext}", md5_hash(url), ext = ext);
    let tmp_path = tmp_dir.join(&filename);
    std::fs::write(&tmp_path, &body)?;

    tracing::info!(
        url = %url,
        path = %tmp_path.display(),
        bytes = body.len(),
        "downloaded URL to temp file"
    );

    Ok(tmp_path)
}

/// Simple md5 hash for unique filenames (not cryptographic).
fn md5_hash(s: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn infer_extension_from_url(url: &str, content_type: &str) -> &'static str {
    // Try content-type first
    if content_type.contains("image/jpeg") || content_type.contains("image/jpg") {
        return "jpg";
    }
    if content_type.contains("image/png") {
        return "png";
    }
    if content_type.contains("image/gif") {
        return "gif";
    }
    if content_type.contains("image/webp") {
        return "webp";
    }
    if content_type.contains("audio/mpeg") || content_type.contains("audio/mp3") {
        return "mp3";
    }
    if content_type.contains("audio/wav") {
        return "wav";
    }
    if content_type.contains("audio/ogg") {
        return "ogg";
    }
    if content_type.contains("audio/m4a") || content_type.contains("audio/mp4") {
        return "m4a";
    }
    if content_type.contains("video/mp4") {
        return "mp4";
    }
    if content_type.contains("video/webm") {
        return "webm";
    }

    // Try URL path extension
    let url_path = url.split('?').next().unwrap_or(url);
    if let Some(dot_pos) = url_path.rfind('.') {
        let ext = &url_path[dot_pos + 1..].to_lowercase();
        match ext.as_str() {
            "jpg" | "jpeg" => "jpg",
            "png" => "png",
            "gif" => "gif",
            "webp" => "webp",
            "mp3" => "mp3",
            "wav" => "wav",
            "ogg" => "ogg",
            "m4a" => "m4a",
            "mp4" => "mp4",
            "webm" => "webm",
            "flac" => "flac",
            _ => "bin",
        }
    } else {
        "bin"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_media_path_requires_full_shape() {
        assert!(is_session_media_path("sessions/abc123/files/photo.png"));
        assert!(is_session_media_path("sessions\\abc123\\files\\photo.png"));
        assert!(is_session_media_path("./sessions/abc123/files/photo.png"));
    }

    #[test]
    fn plain_sessions_prefix_is_not_hijacked() {
        // A workspace's own `sessions/` directory (e.g. a PHP/Express session
        // store) must not be misrouted to the data dir just because it starts
        // with "sessions/".
        assert!(!is_session_media_path("sessions/config.php"));
        assert!(!is_session_media_path("sessions/abc123/photo.png"));
        assert!(!is_session_media_path("sessions//files/photo.png"));
        assert!(!is_session_media_path("sessions/abc123/files/"));
        assert!(!is_session_media_path("sessionsfoo/abc123/files/photo.png"));
    }
}
