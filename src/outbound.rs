//! Typed outbound operations for the durable job queue.
//!
//! Each outbound op (text, media, reaction, edit, etc.) is serialized as
//! `op_kind` + `payload_json` in SQLite. Media bytes are stored in `payload_blob`.

use serde::{Deserialize, Serialize};

/// Identifies the type of outbound operation in the job queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboundOpKind {
    Text,
    Reply,
    Image,
    Video,
    Audio,
    Document,
    Sticker,
    Location,
    Contact,
    Reaction,
    Unreact,
    Edit,
    Revoke,
    Poll,
    Forward,
    ViewOnceImage,
    ViewOnceVideo,
}

impl OutboundOpKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Reply => "reply",
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Document => "document",
            Self::Sticker => "sticker",
            Self::Location => "location",
            Self::Contact => "contact",
            Self::Reaction => "reaction",
            Self::Unreact => "unreact",
            Self::Edit => "edit",
            Self::Revoke => "revoke",
            Self::Poll => "poll",
            Self::Forward => "forward",
            Self::ViewOnceImage => "view_once_image",
            Self::ViewOnceVideo => "view_once_video",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "text" => Some(Self::Text),
            "reply" => Some(Self::Reply),
            "image" => Some(Self::Image),
            "video" => Some(Self::Video),
            "audio" => Some(Self::Audio),
            "document" => Some(Self::Document),
            "sticker" => Some(Self::Sticker),
            "location" => Some(Self::Location),
            "contact" => Some(Self::Contact),
            "reaction" => Some(Self::Reaction),
            "unreact" => Some(Self::Unreact),
            "edit" => Some(Self::Edit),
            "revoke" => Some(Self::Revoke),
            "poll" => Some(Self::Poll),
            "forward" => Some(Self::Forward),
            "view_once_image" => Some(Self::ViewOnceImage),
            "view_once_video" => Some(Self::ViewOnceVideo),
            _ => None,
        }
    }
}

/// Payload for text message ops.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextPayload {
    pub text: String,
}

/// Payload for reply ops.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplyPayload {
    pub text: String,
    pub reply_to_id: String,
    pub reply_to_sender: String,
}

/// Payload for media ops (image, video, audio, document, sticker, view-once).
/// Actual bytes stored in `payload_blob` column.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaPayload {
    pub mime: String,
    pub caption: Option<String>,
    pub filename: Option<String>,
    /// Audio duration in seconds (voice notes).
    pub seconds: Option<u32>,
}

/// Payload for location ops.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocationPayload {
    pub lat: f64,
    pub lon: f64,
    pub name: Option<String>,
    pub address: Option<String>,
}

/// Payload for contact ops.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContactPayload {
    pub display_name: String,
    pub vcard: String,
}

/// Payload for reaction ops.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionPayload {
    pub target_message_id: String,
    pub emoji: String,
    pub target_sender_jid: Option<String>,
    pub target_is_from_me: bool,
}

/// Payload for edit ops.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditPayload {
    pub message_id: String,
    pub new_text: String,
}

/// Payload for revoke ops.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevokePayload {
    pub message_id: String,
}

/// Payload for poll ops.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollPayload {
    pub question: String,
    pub options: Vec<String>,
    pub selectable_count: u32,
}

/// Payload for forward ops. Protobuf bytes stored in `payload_blob`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForwardPayload {
    pub original_msg_id: String,
}

/// A queued outbound job row from the database.
#[derive(Debug, Clone)]
pub struct OutboundJobRow {
    pub id: i64,
    pub jid: String,
    pub op_kind: String,
    pub payload_json: String,
    pub payload_blob: Option<Vec<u8>>,
    pub retries: i32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_op_kind_roundtrip() {
        for kind in [
            OutboundOpKind::Text,
            OutboundOpKind::Reply,
            OutboundOpKind::Image,
            OutboundOpKind::Reaction,
            OutboundOpKind::Forward,
        ] {
            assert_eq!(OutboundOpKind::from_str(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn test_text_payload_serde() {
        let p = TextPayload { text: "hello".to_string() };
        let json = serde_json::to_string(&p).unwrap();
        let p2: TextPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(p2.text, "hello");
    }
}
