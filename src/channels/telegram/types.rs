use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    #[serde(default)]
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub edited_message: Option<Message>,
    #[serde(default)]
    pub callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PhotoSize {
    #[serde(default)]
    pub file_id: String,
    #[serde(default)]
    pub file_unique_id: String,
    #[serde(default)]
    pub width: i32,
    #[serde(default)]
    pub height: i32,
    #[serde(default)]
    pub file_size: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    #[serde(default)]
    pub message_id: i64,
    #[serde(default)]
    pub message_thread_id: Option<i64>,
    #[serde(default)]
    pub from: Option<User>,
    #[serde(default)]
    pub chat: Chat,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub caption: Option<String>,
    #[serde(default)]
    pub photo: Option<Vec<PhotoSize>>,
    #[serde(default)]
    pub voice: Option<Voice>,
    #[serde(default)]
    pub audio: Option<Audio>,
    #[serde(default)]
    pub video: Option<Video>,
    #[serde(default)]
    pub video_note: Option<VideoNote>,
    #[serde(default)]
    pub document: Option<Document>,
    #[serde(default)]
    pub forward_from: Option<User>,
    #[serde(default)]
    pub forward_from_chat: Option<Chat>,
    #[serde(default)]
    pub forward_sender_name: Option<String>,
    #[serde(default)]
    pub forward_date: Option<i64>,
    #[serde(default)]
    pub reply_to_message: Option<Box<Message>>,
}

/// Telegram video message.
#[derive(Debug, Clone, Deserialize)]
pub struct Video {
    #[serde(default)]
    pub file_id: String,
    #[serde(default)]
    pub mime_type: Option<String>,
}

/// Telegram generic document (any non-media file).
#[derive(Debug, Clone, Deserialize)]
pub struct Document {
    #[serde(default)]
    pub file_id: String,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub file_name: Option<String>,
}

/// Telegram video note (round video message).
#[derive(Debug, Clone, Deserialize)]
pub struct VideoNote {
    #[serde(default)]
    pub file_id: String,
}

/// Telegram voice message (OGG/Opus). `mime_type` is usually "audio/ogg".
#[derive(Debug, Clone, Deserialize)]
pub struct Voice {
    #[serde(default)]
    pub file_id: String,
    #[serde(default)]
    pub mime_type: Option<String>,
}

/// Telegram audio (music/file). Carries an explicit MIME type when known.
#[derive(Debug, Clone, Deserialize)]
pub struct Audio {
    #[serde(default)]
    pub file_id: String,
    #[serde(default)]
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub first_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Chat {
    #[serde(default)]
    pub id: i64,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CallbackQuery {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub from: Option<User>,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub data: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SendMessageRequest {
    #[serde(rename = "chat_id")]
    pub chat_id: String,
    #[serde(rename = "message_thread_id", skip_serializing_if = "Option::is_none")]
    pub message_thread_id: Option<String>,
    #[serde(default)]
    pub text: String,
    #[serde(rename = "parse_mode", skip_serializing_if = "Option::is_none")]
    pub parse_mode: Option<String>,
    #[serde(rename = "reply_markup", skip_serializing_if = "Option::is_none")]
    pub reply_markup: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SendChatActionRequest {
    #[serde(rename = "chat_id")]
    pub chat_id: String,
    #[serde(rename = "message_thread_id", skip_serializing_if = "Option::is_none")]
    pub message_thread_id: Option<String>,
    #[serde(rename = "action")]
    pub action: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GetUpdatesResponse {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub result: Vec<Update>,
}
