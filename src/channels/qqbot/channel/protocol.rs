// ── Constants ─────────────────────────────────────────────────────────────────

/// WebSocket gateway URL endpoint.
pub const GATEWAY_URL: &str = "https://api.sgroup.qq.com/gateway/bot";

/// Token endpoint.
pub const TOKEN_URL: &str = "https://bots.qq.com/app/getAppAccessToken";

/// REST API base for v2 messages.
pub const API_BASE: &str = "https://api.sgroup.qq.com";

/// WebSocket intents:
///   PUBLIC_GUILD_MESSAGES   = 1 << 30 = 1073741824
///   GROUP_AT_MESSAGE_CREATE = 1 << 25 = 33554432
///   C2C_MESSAGE_CREATE      = 1 << 25 = 33554432
///   DIRECT_MESSAGE          = 1 << 12 = 4096
///   INTERACTION             = 1 << 26 = 67108864
pub const INTENTS: u32 = (1 << 30) | (1 << 25) | (1 << 12) | (1 << 26);

/// WebSocket opcodes.
pub const OP_RESUME: u32 = 6;
pub const OP_HELLO: u32 = 10;
pub const OP_IDENTIFY: u32 = 2;
pub const OP_HEARTBEAT: u32 = 1;
pub const OP_HEARTBEAT_ACK: u32 = 11;
pub const OP_DISPATCH: u32 = 0;
pub const OP_RECONNECT: u32 = 7;
pub const OP_INVALID_SESSION: u32 = 9;


/// Derive a file extension from a QQ attachment content_type string.
/// QQ sends content_type as either a category ("image", "video") or a MIME
/// ("image/jpeg", "video/mp4"). We need the extension for temp file paths so
/// that downstream modality detection (which falls back to extension) works.
pub(super) fn mime_ext_from_content_type(ct: &str) -> &'static str {
    match ct {
        "image" | "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "video" | "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/ogg" => "ogg",
        "audio/wav" => "wav",
        _ => "bin",
    }
}

/// Build User-Agent string for QQ Bot HTTP requests.
pub fn user_agent() -> String {
    let os = std::env::consts::OS;
    format!("MyClaw/{} (Rust; {})", env!("MYCLAW_VERSION"), os)
}
