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
    StatusText,
    StatusImage,
    StatusVideo,
    StatusRevoke,
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
            Self::StatusText => "status_text",
            Self::StatusImage => "status_image",
            Self::StatusVideo => "status_video",
            Self::StatusRevoke => "status_revoke",
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
            "status_text" => Some(Self::StatusText),
            "status_image" => Some(Self::StatusImage),
            "status_video" => Some(Self::StatusVideo),
            "status_revoke" => Some(Self::StatusRevoke),
            _ => None,
        }
    }
}

/// Link preview metadata for outbound text messages.
/// When present, the message is sent as ExtendedTextMessage with a preview card.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkPreview {
    /// The URL being previewed (must appear in the message text).
    pub url: String,
    pub title: Option<String>,
    pub description: Option<String>,
    /// Base64-encoded JPEG thumbnail (≤320px recommended).
    pub thumbnail_b64: Option<String>,
}

/// Payload for text message ops.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextPayload {
    pub text: String,
    /// JIDs to @mention (e.g. ["user@s.whatsapp.net"]). Empty = no mentions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<String>,
    /// Optional link preview — attaches a preview card to the message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_preview: Option<LinkPreview>,
}

/// Payload for reply ops.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplyPayload {
    pub text: String,
    pub reply_to_id: String,
    pub reply_to_sender: String,
    /// JIDs to @mention in the reply.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<String>,
    /// If true, payload_blob contains the quoted message proto for quote header display.
    #[serde(default)]
    pub has_quoted_message: bool,
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
    /// If true (default), audio is sent as push-to-talk voice note.
    /// Set to false to send as a regular audio file attachment.
    #[serde(default = "default_true")]
    pub is_voice_note: bool,
}

fn default_true() -> bool { true }

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

/// Payload for status/story text posts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusTextPayload {
    pub recipients: Vec<String>,
    pub text: String,
    pub background_argb: u32,
    pub font: i32,
    pub privacy: Option<String>,
}

/// Payload for status/story media posts (image or video).
/// Actual bytes stored in `payload_blob` column.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusMediaPayload {
    pub recipients: Vec<String>,
    pub mime: String,
    pub caption: Option<String>,
    /// Video duration in seconds. 0 for images.
    pub seconds: u32,
    pub privacy: Option<String>,
}

/// Payload for status/story revocation.
/// Recipients MUST match the original post — wa-rs encrypts revoke to same device set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusRevokePayload {
    pub recipients: Vec<String>,
    pub message_id: String,
    pub privacy: Option<String>,
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

/// Outcome of executing an outbound job.
pub struct ExecOutcome {
    /// WhatsApp message ID (from send_message, edit_message, or revoke_message).
    pub wa_message_id: Option<String>,
    /// Additional side effects (e.g. poll key storage).
    pub poll_key: Option<PollKeyInfo>,
}

/// Poll key + options to store after send for vote decryption.
pub struct PollKeyInfo {
    pub enc_key: Vec<u8>,
    pub options: Vec<String>,
}

/// Parse a privacy string into a StatusPrivacySetting.
pub fn parse_status_privacy(s: Option<String>) -> whatsapp_rust::StatusPrivacySetting {
    match s.as_deref() {
        Some("allowlist") => whatsapp_rust::StatusPrivacySetting::AllowList,
        Some("denylist") => whatsapp_rust::StatusPrivacySetting::DenyList,
        _ => whatsapp_rust::StatusPrivacySetting::Contacts,
    }
}

/// Parse a list of phone number strings into Vec<Jid>.
fn parse_recipient_jids(recipients: &[String]) -> anyhow::Result<Vec<wacore_binary::jid::Jid>> {
    use std::str::FromStr;
    recipients
        .iter()
        .map(|r| {
            let normalized = r.trim().replace('+', "").replace(' ', "").replace('-', "");
            wacore_binary::jid::Jid::from_str(&format!("{normalized}@s.whatsapp.net"))
                .map_err(|e| anyhow::anyhow!("bad recipient JID '{r}': {e}"))
        })
        .collect()
}

/// Build and send a wa::Message from a job row. Returns the WA message ID if applicable.
///
/// Media ops upload bytes to WhatsApp servers before sending. Edit/revoke use
/// specialised client methods. All other ops build a `wa::Message` and call
/// `client.send_message()`.
pub async fn execute_job(
    row: &OutboundJobRow,
    target: &wacore_binary::jid::Jid,
    client: &std::sync::Arc<whatsapp_rust::Client>,
) -> anyhow::Result<ExecOutcome> {
    use whatsapp_rust::download::MediaType;
    use whatsapp_rust::UploadOptions;
    use waproto::whatsapp as wa;

    let kind = OutboundOpKind::from_str(&row.op_kind)
        .ok_or_else(|| anyhow::anyhow!("unknown op_kind: {}", row.op_kind))?;

    match kind {
        OutboundOpKind::Text => {
            let p: TextPayload = serde_json::from_str(&row.payload_json)?;
            let has_mentions = !p.mentions.is_empty();
            let has_preview = p.link_preview.is_some();

            let msg = if !has_mentions && !has_preview {
                // Simple text — no mentions, no preview
                wa::Message { conversation: Some(p.text), ..Default::default() }
            } else {
                // Extended text — needed for mentions and/or link preview
                let context_info = if has_mentions {
                    Some(Box::new(wa::ContextInfo {
                        mentioned_jid: p.mentions,
                        ..Default::default()
                    }))
                } else {
                    None
                };

                let (matched_text, title, description, jpeg_thumbnail, preview_type) =
                    if let Some(ref lp) = p.link_preview {
                        let thumb = lp.thumbnail_b64.as_deref().and_then(|b64| {
                            use base64::Engine;
                            base64::engine::general_purpose::STANDARD.decode(b64).ok()
                        });
                        let pt = if thumb.is_some() { Some(5) } else { Some(0) }; // 5 = Image
                        (Some(lp.url.clone()), lp.title.clone(), lp.description.clone(), thumb, pt)
                    } else {
                        (None, None, None, None, None)
                    };

                wa::Message {
                    extended_text_message: Some(Box::new(wa::message::ExtendedTextMessage {
                        text: Some(p.text),
                        matched_text,
                        title,
                        description,
                        jpeg_thumbnail,
                        preview_type,
                        context_info,
                        ..Default::default()
                    })),
                    ..Default::default()
                }
            };
            let result = client.send_message(target.clone(), msg).await
                .map_err(|e| anyhow::anyhow!("send text: {e}"))?;
            Ok(ExecOutcome { wa_message_id: Some(result.message_id), poll_key: None })
        }

        OutboundOpKind::Reply => {
            let p: ReplyPayload = serde_json::from_str(&row.payload_json)?;
            // Decode quoted message proto from payload_blob if available (for quote header)
            let quoted_message = if p.has_quoted_message {
                row.payload_blob.as_ref().and_then(|blob| {
                    prost::Message::decode(blob.as_slice()).ok().map(Box::new)
                })
            } else {
                None
            };
            let context_info = wa::ContextInfo {
                stanza_id: Some(p.reply_to_id),
                participant: Some(p.reply_to_sender),
                quoted_message,
                mentioned_jid: p.mentions,
                ..Default::default()
            };
            let msg = wa::Message {
                extended_text_message: Some(Box::new(wa::message::ExtendedTextMessage {
                    text: Some(p.text),
                    context_info: Some(Box::new(context_info)),
                    ..Default::default()
                })),
                ..Default::default()
            };
            let result = client.send_message(target.clone(), msg).await
                .map_err(|e| anyhow::anyhow!("send reply: {e}"))?;
            Ok(ExecOutcome { wa_message_id: Some(result.message_id), poll_key: None })
        }

        OutboundOpKind::Image | OutboundOpKind::ViewOnceImage => {
            let p: MediaPayload = serde_json::from_str(&row.payload_json)?;
            let data = row.payload_blob.clone()
                .ok_or_else(|| anyhow::anyhow!("image op missing payload_blob"))?;
            let upload = client.upload(data, MediaType::Image, UploadOptions::default()).await
                .map_err(|e| anyhow::anyhow!("image upload: {e}"))?;
            let msg = wa::Message {
                image_message: Some(Box::new(wa::message::ImageMessage {
                    mimetype: Some(p.mime),
                    caption: p.caption,
                    url: Some(upload.url),
                    direct_path: Some(upload.direct_path),
                    media_key: Some(upload.media_key),
                    file_enc_sha256: Some(upload.file_enc_sha256),
                    file_sha256: Some(upload.file_sha256),
                    file_length: Some(upload.file_length),
                    view_once: if kind == OutboundOpKind::ViewOnceImage { Some(true) } else { None },
                    ..Default::default()
                })),
                ..Default::default()
            };
            let result = client.send_message(target.clone(), msg).await
                .map_err(|e| anyhow::anyhow!("send image: {e}"))?;
            Ok(ExecOutcome { wa_message_id: Some(result.message_id), poll_key: None })
        }

        OutboundOpKind::Video | OutboundOpKind::ViewOnceVideo => {
            let p: MediaPayload = serde_json::from_str(&row.payload_json)?;
            let data = row.payload_blob.clone()
                .ok_or_else(|| anyhow::anyhow!("video op missing payload_blob"))?;
            let upload = client.upload(data, MediaType::Video, UploadOptions::default()).await
                .map_err(|e| anyhow::anyhow!("video upload: {e}"))?;
            let msg = wa::Message {
                video_message: Some(Box::new(wa::message::VideoMessage {
                    mimetype: Some(p.mime),
                    caption: p.caption,
                    url: Some(upload.url),
                    direct_path: Some(upload.direct_path),
                    media_key: Some(upload.media_key),
                    file_enc_sha256: Some(upload.file_enc_sha256),
                    file_sha256: Some(upload.file_sha256),
                    file_length: Some(upload.file_length),
                    view_once: if kind == OutboundOpKind::ViewOnceVideo { Some(true) } else { None },
                    ..Default::default()
                })),
                ..Default::default()
            };
            let result = client.send_message(target.clone(), msg).await
                .map_err(|e| anyhow::anyhow!("send video: {e}"))?;
            Ok(ExecOutcome { wa_message_id: Some(result.message_id), poll_key: None })
        }

        OutboundOpKind::Audio => {
            let p: MediaPayload = serde_json::from_str(&row.payload_json)?;
            let data = row.payload_blob.clone()
                .ok_or_else(|| anyhow::anyhow!("audio op missing payload_blob"))?;
            if p.is_voice_note {
                let _ = client.chatstate().send_recording(target).await;
            }
            let upload = client.upload(data, MediaType::Audio, UploadOptions::default()).await
                .map_err(|e| anyhow::anyhow!("audio upload: {e}"))?;
            let msg = wa::Message {
                audio_message: Some(Box::new(wa::message::AudioMessage {
                    mimetype: Some(p.mime),
                    ptt: Some(p.is_voice_note),
                    seconds: p.seconds,
                    url: Some(upload.url),
                    direct_path: Some(upload.direct_path),
                    media_key: Some(upload.media_key),
                    file_enc_sha256: Some(upload.file_enc_sha256),
                    file_sha256: Some(upload.file_sha256),
                    file_length: Some(upload.file_length),
                    ..Default::default()
                })),
                ..Default::default()
            };
            let send_result = client.send_message(target.clone(), msg).await
                .map_err(|e| anyhow::anyhow!("send audio: {e}"));
            if p.is_voice_note {
                let _ = client.chatstate().send_paused(target).await;
            }
            let result = send_result?;
            Ok(ExecOutcome { wa_message_id: Some(result.message_id), poll_key: None })
        }

        OutboundOpKind::Document => {
            let p: MediaPayload = serde_json::from_str(&row.payload_json)?;
            let data = row.payload_blob.clone()
                .ok_or_else(|| anyhow::anyhow!("document op missing payload_blob"))?;
            let upload = client.upload(data, MediaType::Document, UploadOptions::default()).await
                .map_err(|e| anyhow::anyhow!("document upload: {e}"))?;
            let msg = wa::Message {
                document_message: Some(Box::new(wa::message::DocumentMessage {
                    mimetype: Some(p.mime),
                    file_name: p.filename,
                    caption: p.caption,
                    url: Some(upload.url),
                    direct_path: Some(upload.direct_path),
                    media_key: Some(upload.media_key),
                    file_enc_sha256: Some(upload.file_enc_sha256),
                    file_sha256: Some(upload.file_sha256),
                    file_length: Some(upload.file_length),
                    ..Default::default()
                })),
                ..Default::default()
            };
            let result = client.send_message(target.clone(), msg).await
                .map_err(|e| anyhow::anyhow!("send document: {e}"))?;
            Ok(ExecOutcome { wa_message_id: Some(result.message_id), poll_key: None })
        }

        OutboundOpKind::Sticker => {
            let p: MediaPayload = serde_json::from_str(&row.payload_json)?;
            let data = row.payload_blob.clone()
                .ok_or_else(|| anyhow::anyhow!("sticker op missing payload_blob"))?;
            let upload = client.upload(data, MediaType::Sticker, UploadOptions::default()).await
                .map_err(|e| anyhow::anyhow!("sticker upload: {e}"))?;
            // Sticker animated flag: use seconds field as a boolean (0 = static, >0 = animated)
            let is_animated = p.seconds.unwrap_or(0) > 0;
            let msg = wa::Message {
                sticker_message: Some(Box::new(wa::message::StickerMessage {
                    mimetype: Some(p.mime),
                    is_animated: Some(is_animated),
                    url: Some(upload.url),
                    direct_path: Some(upload.direct_path),
                    media_key: Some(upload.media_key),
                    file_enc_sha256: Some(upload.file_enc_sha256),
                    file_sha256: Some(upload.file_sha256),
                    file_length: Some(upload.file_length),
                    ..Default::default()
                })),
                ..Default::default()
            };
            let result = client.send_message(target.clone(), msg).await
                .map_err(|e| anyhow::anyhow!("send sticker: {e}"))?;
            Ok(ExecOutcome { wa_message_id: Some(result.message_id), poll_key: None })
        }

        OutboundOpKind::Location => {
            let p: LocationPayload = serde_json::from_str(&row.payload_json)?;
            let msg = wa::Message {
                location_message: Some(Box::new(wa::message::LocationMessage {
                    degrees_latitude: Some(p.lat),
                    degrees_longitude: Some(p.lon),
                    name: p.name,
                    address: p.address,
                    ..Default::default()
                })),
                ..Default::default()
            };
            let result = client.send_message(target.clone(), msg).await
                .map_err(|e| anyhow::anyhow!("send location: {e}"))?;
            Ok(ExecOutcome { wa_message_id: Some(result.message_id), poll_key: None })
        }

        OutboundOpKind::Contact => {
            let p: ContactPayload = serde_json::from_str(&row.payload_json)?;
            let msg = wa::Message {
                contact_message: Some(Box::new(wa::message::ContactMessage {
                    display_name: Some(p.display_name),
                    vcard: Some(p.vcard),
                    ..Default::default()
                })),
                ..Default::default()
            };
            let result = client.send_message(target.clone(), msg).await
                .map_err(|e| anyhow::anyhow!("send contact: {e}"))?;
            Ok(ExecOutcome { wa_message_id: Some(result.message_id), poll_key: None })
        }

        OutboundOpKind::Reaction | OutboundOpKind::Unreact => {
            let p: ReactionPayload = serde_json::from_str(&row.payload_json)?;
            let emoji = if kind == OutboundOpKind::Unreact { String::new() } else { p.emoji };
            // In groups, participant is always required — even for self-authored messages.
            let is_group = target.to_string().ends_with("@g.us");
            let participant = if is_group {
                p.target_sender_jid
            } else {
                None
            };
            let msg = wa::Message {
                reaction_message: Some(wa::message::ReactionMessage {
                    key: Some(wa::MessageKey {
                        remote_jid: Some(target.to_string()),
                        id: Some(p.target_message_id),
                        from_me: Some(p.target_is_from_me),
                        participant,
                    }),
                    text: Some(emoji),
                    sender_timestamp_ms: Some(chrono::Utc::now().timestamp_millis()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let result = client.send_message(target.clone(), msg).await
                .map_err(|e| anyhow::anyhow!("send reaction: {e}"))?;
            Ok(ExecOutcome { wa_message_id: Some(result.message_id), poll_key: None })
        }

        OutboundOpKind::Edit => {
            let p: EditPayload = serde_json::from_str(&row.payload_json)?;
            // Use extended_text_message so edits work on messages that originally had
            // mentions, link previews, or other extended features.
            let new_content = wa::Message {
                extended_text_message: Some(Box::new(wa::message::ExtendedTextMessage {
                    text: Some(p.new_text),
                    ..Default::default()
                })),
                ..Default::default()
            };
            client.edit_message(target.clone(), p.message_id, new_content).await
                .map_err(|e| anyhow::anyhow!("edit: {e}"))?;
            Ok(ExecOutcome { wa_message_id: None, poll_key: None })
        }

        OutboundOpKind::Revoke => {
            let p: RevokePayload = serde_json::from_str(&row.payload_json)?;
            client.revoke_message(target.clone(), p.message_id, whatsapp_rust::RevokeType::Sender).await
                .map_err(|e| anyhow::anyhow!("revoke: {e}"))?;
            Ok(ExecOutcome { wa_message_id: None, poll_key: None })
        }

        OutboundOpKind::Poll => {
            let p: PollPayload = serde_json::from_str(&row.payload_json)?;
            let enc_key = crate::polls::generate_poll_key();
            let poll_options: Vec<wa::message::poll_creation_message::Option> = p
                .options
                .iter()
                .map(|name| wa::message::poll_creation_message::Option {
                    option_name: Some(name.clone()),
                    option_hash: None,
                })
                .collect();
            let msg = wa::Message {
                poll_creation_message: Some(Box::new(wa::message::PollCreationMessage {
                    enc_key: Some(enc_key.to_vec()),
                    name: Some(p.question),
                    options: poll_options,
                    selectable_options_count: Some(p.selectable_count),
                    ..Default::default()
                })),
                ..Default::default()
            };
            let result = client.send_message(target.clone(), msg).await
                .map_err(|e| anyhow::anyhow!("send poll: {e}"))?;
            Ok(ExecOutcome {
                wa_message_id: Some(result.message_id),
                poll_key: Some(PollKeyInfo { enc_key: enc_key.to_vec(), options: p.options }),
            })
        }

        OutboundOpKind::Forward => {
            let proto_bytes = row.payload_blob.clone()
                .ok_or_else(|| anyhow::anyhow!("forward op missing payload_blob (protobuf)"))?;
            let original: wa::Message = prost::Message::decode(proto_bytes.as_slice())
                .map_err(|e| anyhow::anyhow!("forward: decode protobuf: {e}"))?;
            let forwarded = crate::bridge::add_forward_context(original);
            let result = client.send_message(target.clone(), forwarded).await
                .map_err(|e| anyhow::anyhow!("forward: {e}"))?;
            Ok(ExecOutcome { wa_message_id: Some(result.message_id), poll_key: None })
        }

        OutboundOpKind::StatusText => {
            let p: StatusTextPayload = serde_json::from_str(&row.payload_json)?;
            let recipients = parse_recipient_jids(&p.recipients)?;
            let opts = whatsapp_rust::StatusSendOptions {
                privacy: parse_status_privacy(p.privacy),
            };
            let result = client.status().send_text(&p.text, p.background_argb, p.font, &recipients, opts).await
                .map_err(|e| anyhow::anyhow!("send status text: {e}"))?;
            Ok(ExecOutcome { wa_message_id: Some(result.message_id), poll_key: None })
        }

        OutboundOpKind::StatusImage => {
            let p: StatusMediaPayload = serde_json::from_str(&row.payload_json)?;
            let data = row.payload_blob.clone()
                .ok_or_else(|| anyhow::anyhow!("status image op missing payload_blob"))?;
            let recipients = parse_recipient_jids(&p.recipients)?;
            let opts = whatsapp_rust::StatusSendOptions {
                privacy: parse_status_privacy(p.privacy),
            };
            let upload = client.upload(data, MediaType::Image, UploadOptions::default()).await
                .map_err(|e| anyhow::anyhow!("status image upload: {e}"))?;
            let result = client.status().send_image(&upload, vec![], p.caption.as_deref(), &recipients, opts).await
                .map_err(|e| anyhow::anyhow!("send status image: {e}"))?;
            Ok(ExecOutcome { wa_message_id: Some(result.message_id), poll_key: None })
        }

        OutboundOpKind::StatusVideo => {
            let p: StatusMediaPayload = serde_json::from_str(&row.payload_json)?;
            let data = row.payload_blob.clone()
                .ok_or_else(|| anyhow::anyhow!("status video op missing payload_blob"))?;
            let recipients = parse_recipient_jids(&p.recipients)?;
            let opts = whatsapp_rust::StatusSendOptions {
                privacy: parse_status_privacy(p.privacy),
            };
            let upload = client.upload(data, MediaType::Video, UploadOptions::default()).await
                .map_err(|e| anyhow::anyhow!("status video upload: {e}"))?;
            let result = client.status().send_video(&upload, vec![], p.seconds, p.caption.as_deref(), &recipients, opts).await
                .map_err(|e| anyhow::anyhow!("send status video: {e}"))?;
            Ok(ExecOutcome { wa_message_id: Some(result.message_id), poll_key: None })
        }

        OutboundOpKind::StatusRevoke => {
            let p: StatusRevokePayload = serde_json::from_str(&row.payload_json)?;
            let recipients = parse_recipient_jids(&p.recipients)?;
            let opts = whatsapp_rust::StatusSendOptions {
                privacy: parse_status_privacy(p.privacy),
            };
            let result = client.status().revoke(p.message_id, &recipients, opts).await
                .map_err(|e| anyhow::anyhow!("revoke status: {e}"))?;
            Ok(ExecOutcome { wa_message_id: Some(result.message_id), poll_key: None })
        }
    }
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
        let p = TextPayload { text: "hello".to_string(), mentions: vec![], link_preview: None };
        let json = serde_json::to_string(&p).unwrap();
        assert!(!json.contains("mentions")); // empty vec skipped
        let p2: TextPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(p2.text, "hello");
        assert!(p2.mentions.is_empty());
    }

    #[test]
    fn test_text_payload_with_mentions_serde() {
        let p = TextPayload {
            text: "Hey @user".to_string(),
            mentions: vec!["user@s.whatsapp.net".to_string()],
            link_preview: None,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("mentions"));
        let p2: TextPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(p2.mentions, vec!["user@s.whatsapp.net"]);
    }

    #[test]
    fn test_text_payload_backward_compat_no_mentions() {
        // Old payloads without mentions field should deserialize with empty vec
        let json = r#"{"text":"hello"}"#;
        let p: TextPayload = serde_json::from_str(json).unwrap();
        assert_eq!(p.text, "hello");
        assert!(p.mentions.is_empty());
    }

    #[test]
    fn test_status_op_kind_roundtrip() {
        for kind in [
            OutboundOpKind::StatusText,
            OutboundOpKind::StatusImage,
            OutboundOpKind::StatusVideo,
            OutboundOpKind::StatusRevoke,
        ] {
            assert_eq!(OutboundOpKind::from_str(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn test_status_text_payload_serde() {
        let p = StatusTextPayload {
            recipients: vec!["15551234567".to_string()],
            text: "Hello from status".to_string(),
            background_argb: 0xFF1E6E4F,
            font: 2,
            privacy: None,
        };
        let json = serde_json::to_string(&p).unwrap();
        let p2: StatusTextPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(p2.text, "Hello from status");
        assert_eq!(p2.recipients.len(), 1);
        assert_eq!(p2.background_argb, 0xFF1E6E4F);
        assert_eq!(p2.font, 2);
        assert!(p2.privacy.is_none());
    }

    #[test]
    fn test_status_media_payload_serde() {
        let p = StatusMediaPayload {
            recipients: vec!["15551234567".to_string(), "15559876543".to_string()],
            mime: "image/jpeg".to_string(),
            caption: Some("My photo".to_string()),
            seconds: 0,
            privacy: Some("contacts".to_string()),
        };
        let json = serde_json::to_string(&p).unwrap();
        let p2: StatusMediaPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(p2.recipients.len(), 2);
        assert_eq!(p2.caption, Some("My photo".to_string()));
        assert_eq!(p2.seconds, 0);
    }

    #[test]
    fn test_status_revoke_payload_serde() {
        let p = StatusRevokePayload {
            recipients: vec!["15551234567".to_string()],
            message_id: "3EB06D00CAB92340790621".to_string(),
            privacy: None,
        };
        let json = serde_json::to_string(&p).unwrap();
        let p2: StatusRevokePayload = serde_json::from_str(&json).unwrap();
        assert_eq!(p2.message_id, "3EB06D00CAB92340790621");
        assert_eq!(p2.recipients.len(), 1);
    }

    #[test]
    fn test_parse_status_privacy() {
        assert_eq!(parse_status_privacy(None).as_str(), "contacts");
        assert_eq!(parse_status_privacy(Some("contacts".to_string())).as_str(), "contacts");
        assert_eq!(parse_status_privacy(Some("allowlist".to_string())).as_str(), "allowlist");
        assert_eq!(parse_status_privacy(Some("denylist".to_string())).as_str(), "denylist");
        assert_eq!(parse_status_privacy(Some("bogus".to_string())).as_str(), "contacts");
    }
}
