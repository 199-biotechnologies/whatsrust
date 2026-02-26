//! WhatsApp bridge — connects to WhatsApp Web via whatsapp-rust (wa-rs).
//!
//! Adapted from ZeroClaw's whatsapp_web.rs but leaner:
//!   - Channel-based architecture (mpsc inbound/outbound)
//!   - Reconnection with exponential backoff
//!   - QR + pair-code pairing
//!   - LID-to-phone sender normalization
//!   - Expanded event handling (ban, outdated, stream errors)

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use parking_lot::Mutex as ParkingMutex;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use wacore::store::traits::ProtocolStore;
use wacore_binary::jid::Jid;
use waproto::whatsapp as wa;

use whatsapp_rust::bot::Bot;
use whatsapp_rust::types::events::Event;
use whatsapp_rust::Client;
use whatsapp_rust_tokio_transport::TokioWebSocketTransportFactory;
use whatsapp_rust::download::{Downloadable, MediaType};
use whatsapp_rust::pair_code::PairCodeOptions;
use whatsapp_rust_ureq_http_client::UreqHttpClient;

use crate::storage::Store;

const OUTBOUND_RETRY_DELAY: Duration = Duration::from_millis(500);
const MAX_OUTBOUND_RETRY_QUEUE: usize = 2048;
/// Max message IDs to remember for dedup (prevents double-processing on reconnect).
const DEDUP_CACHE_CAPACITY: usize = 4096;

// ---------------------------------------------------------------------------
// Dedup cache — bounded ring-buffer set to prevent double-processing
// ---------------------------------------------------------------------------

/// A bounded set that evicts the oldest entry when full.
/// Uses a VecDeque as a ring buffer + HashSet for O(1) lookup.
struct DedupCache {
    order: VecDeque<String>,
    seen: HashSet<String>,
    capacity: usize,
}

impl DedupCache {
    fn new(capacity: usize) -> Self {
        Self {
            order: VecDeque::with_capacity(capacity),
            seen: HashSet::with_capacity(capacity),
            capacity,
        }
    }

    /// Returns true if the ID was already seen. Otherwise inserts it.
    fn check_and_insert(&mut self, id: &str) -> bool {
        if self.seen.contains(id) {
            return true;
        }
        if self.order.len() >= self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        let owned = id.to_string();
        self.seen.insert(owned.clone());
        self.order.push_back(owned);
        false
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeState {
    Disconnected,
    Pairing,
    Connected,
    Reconnecting,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionAction {
    Retry,
    Stop,
}

/// Content variants for inbound WhatsApp messages.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields consumed by bot integrations, not the REPL
pub enum InboundContent {
    /// Plain text or extended text message.
    Text {
        body: String,
    },
    /// Image with downloaded bytes.
    Image {
        data: Vec<u8>,
        mime: String,
        caption: Option<String>,
    },
    /// Audio (voice note or file) with downloaded bytes.
    Audio {
        data: Vec<u8>,
        mime: String,
        seconds: Option<u32>,
        is_voice: bool,
    },
    /// Video with downloaded bytes.
    Video {
        data: Vec<u8>,
        mime: String,
        caption: Option<String>,
    },
    /// Document/file with downloaded bytes.
    Document {
        data: Vec<u8>,
        mime: String,
        filename: String,
    },
    /// Sticker with downloaded bytes.
    Sticker {
        data: Vec<u8>,
        mime: String,
        is_animated: bool,
    },
    /// Location pin.
    Location {
        lat: f64,
        lon: f64,
        name: Option<String>,
        address: Option<String>,
    },
    /// Contact card (vCard).
    Contact {
        display_name: String,
        vcard: String,
    },
    /// Emoji reaction on a message.
    Reaction {
        target_id: String,
        emoji: String,
    },
    /// Someone edited their message.
    Edit {
        target_id: String,
        new_text: String,
    },
    /// Someone revoked (deleted) a message.
    Revoke {
        target_id: String,
    },
}

impl InboundContent {
    /// Short label for logging.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Text { .. } => "text",
            Self::Image { .. } => "image",
            Self::Audio { .. } => "audio",
            Self::Video { .. } => "video",
            Self::Document { .. } => "document",
            Self::Sticker { .. } => "sticker",
            Self::Location { .. } => "location",
            Self::Contact { .. } => "contact",
            Self::Reaction { .. } => "reaction",
            Self::Edit { .. } => "edit",
            Self::Revoke { .. } => "revoke",
        }
    }

    /// Extract a human-readable summary for display.
    pub fn display_text(&self) -> String {
        match self {
            Self::Text { body } => body.clone(),
            Self::Image { caption, data, .. } => {
                let size = format_size(data.len());
                match caption {
                    Some(c) => format!("[image {size}] {c}"),
                    None => format!("[image {size}]"),
                }
            }
            Self::Audio { data, seconds, is_voice, .. } => {
                let size = format_size(data.len());
                let kind = if *is_voice { "voice" } else { "audio" };
                match seconds {
                    Some(s) => format!("[{kind} {s}s {size}]"),
                    None => format!("[{kind} {size}]"),
                }
            }
            Self::Video { caption, data, .. } => {
                let size = format_size(data.len());
                match caption {
                    Some(c) => format!("[video {size}] {c}"),
                    None => format!("[video {size}]"),
                }
            }
            Self::Document { filename, data, .. } => {
                let size = format_size(data.len());
                format!("[doc: {filename} {size}]")
            }
            Self::Sticker { is_animated, data, .. } => {
                let size = format_size(data.len());
                if *is_animated {
                    format!("[animated sticker {size}]")
                } else {
                    format!("[sticker {size}]")
                }
            }
            Self::Location { lat, lon, name, .. } => match name {
                Some(n) => format!("[location: {n} ({lat:.5},{lon:.5})]"),
                None => format!("[location: {lat:.5},{lon:.5}]"),
            },
            Self::Contact { display_name, .. } => format!("[contact: {display_name}]"),
            Self::Reaction { emoji, target_id } => format!("[react {emoji} on {target_id}]"),
            Self::Edit { target_id, new_text } => format!("[edit {target_id}] {new_text}"),
            Self::Revoke { target_id } => format!("[deleted {target_id}]"),
        }
    }
}

fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields consumed by bot integrations, not the REPL
pub struct WhatsAppInbound {
    /// Which bridge instance received this message (for multi-number routing).
    pub bridge_id: String,
    /// Chat JID (group or individual)
    pub jid: String,
    /// Message ID (for replying, editing, reacting)
    pub id: String,
    /// Message content
    pub content: InboundContent,
    /// Sender — resolved to phone number when possible, raw JID otherwise
    pub sender: String,
    /// Raw sender JID (may be LID)
    pub sender_raw: String,
    /// Unix timestamp
    pub timestamp: i64,
    /// If this message is a reply, the ID of the quoted message
    pub reply_to: Option<String>,
    /// Whether this is from our own account
    pub is_from_me: bool,
}

/// Maximum retries before dropping an outbound message.
const MAX_OUTBOUND_RETRIES: u32 = 3;

#[derive(Debug, Clone)]
pub struct WhatsAppOutbound {
    pub jid: String,
    pub text: String,
    retries: u32,
}

pub struct BridgeConfig {
    /// Unique identifier for this bridge instance (for multi-number routing).
    pub bridge_id: String,
    pub db_path: PathBuf,
    /// Phone number for pair-code linking (e.g. "15551234567"). If None, uses QR.
    pub pair_phone: Option<String>,
    /// Maximum reconnection delay (caps exponential backoff)
    pub max_reconnect_delay: Duration,
    /// Skip history sync on connect (prevents "deaf client" bug with large offline queues)
    pub skip_history_sync: bool,
    /// Automatically send read receipts for inbound messages after enqueue.
    pub auto_mark_read: bool,
    /// If non-empty, only process inbound messages from these phone numbers (digits only).
    pub allowed_numbers: Vec<String>,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            bridge_id: "default".to_string(),
            db_path: PathBuf::from("whatsapp.db"),
            pair_phone: None,
            max_reconnect_delay: Duration::from_secs(60),
            skip_history_sync: true,
            auto_mark_read: true,
            allowed_numbers: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// WhatsAppBridge — public handle for the consumer
// ---------------------------------------------------------------------------

pub struct WhatsAppBridge {
    #[allow(dead_code)] // Used by bot integrations via the outbound channel
    outbound_tx: mpsc::Sender<WhatsAppOutbound>,
    state_rx: watch::Receiver<BridgeState>,
    cancel: CancellationToken,
    client_handle: Arc<ParkingMutex<Option<Arc<Client>>>>,
}

impl WhatsAppBridge {
    /// Start the bridge. Returns immediately — the bot connects in the background.
    /// Use `state()` to monitor connection progress.
    pub fn start(
        config: BridgeConfig,
        inbound_tx: mpsc::Sender<WhatsAppInbound>,
        cancel: CancellationToken,
    ) -> Self {
        let (state_tx, state_rx) = watch::channel(BridgeState::Disconnected);
        let (outbound_tx, outbound_rx) = mpsc::channel::<WhatsAppOutbound>(256);
        let client_handle: Arc<ParkingMutex<Option<Arc<Client>>>> =
            Arc::new(ParkingMutex::new(None));

        let cancel_clone = cancel.clone();
        let ch = client_handle.clone();
        tokio::spawn(async move {
            if let Err(e) =
                run_bridge(config, inbound_tx, outbound_rx, state_tx, cancel_clone, ch).await
            {
                error!(error = %e, "WhatsApp bridge exited with error");
            }
        });

        Self {
            outbound_tx,
            state_rx,
            cancel,
            client_handle,
        }
    }

    #[allow(dead_code)] // Used by bot integrations via the outbound channel
    pub async fn send_message(&self, jid: &str, text: &str) -> Result<()> {
        self.outbound_tx
            .send(WhatsAppOutbound {
                jid: jid.to_string(),
                text: text.to_string(),
                retries: 0,
            })
            .await
            .context("outbound channel closed")
    }

    pub fn state(&self) -> BridgeState {
        *self.state_rx.borrow()
    }

    pub fn stop(&self) {
        self.cancel.cancel();
    }

    pub async fn wait_stopped(&self, timeout: Duration) -> bool {
        let mut rx = self.state_rx.clone();
        let wait = async move {
            loop {
                if *rx.borrow() == BridgeState::Stopped {
                    return true;
                }
                if rx.changed().await.is_err() {
                    return false;
                }
            }
        };
        tokio::time::timeout(timeout, wait).await.unwrap_or(false)
    }

    /// Send a "typing" indicator to a chat.
    pub async fn start_typing(&self, jid: &str) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        client
            .chatstate()
            .send_composing(&target)
            .await
            .map_err(|e| anyhow::anyhow!("chatstate error: {e}"))
    }

    /// Cancel typing indicator for a chat.
    pub async fn stop_typing(&self, jid: &str) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        client
            .chatstate()
            .send_paused(&target)
            .await
            .map_err(|e| anyhow::anyhow!("chatstate error: {e}"))
    }

    /// Check if the bridge has an active WhatsApp connection.
    pub fn is_connected(&self) -> bool {
        self.state() == BridgeState::Connected && get_client_handle(&self.client_handle).is_some()
    }

    /// Send an image file to a chat.
    pub async fn send_image(
        &self,
        jid: &str,
        data: Vec<u8>,
        mime: &str,
        caption: Option<&str>,
    ) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let upload = client
            .upload(data, MediaType::Image)
            .await
            .context("image upload failed")?;
        let msg = wa::Message {
            image_message: Some(Box::new(wa::message::ImageMessage {
                mimetype: Some(mime.to_string()),
                caption: caption.map(|c| c.to_string()),
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
        client
            .send_message(target, msg)
            .await
            .map_err(|e| anyhow::anyhow!("send image failed: {e}"))?;
        Ok(())
    }

    /// Send an audio file as a voice note (PTT).
    pub async fn send_audio(
        &self,
        jid: &str,
        data: Vec<u8>,
        mime: &str,
        seconds: Option<u32>,
    ) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let upload = client
            .upload(data, MediaType::Audio)
            .await
            .context("audio upload failed")?;
        let msg = wa::Message {
            audio_message: Some(Box::new(wa::message::AudioMessage {
                mimetype: Some(mime.to_string()),
                ptt: Some(true),
                seconds,
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
        client
            .send_message(target, msg)
            .await
            .map_err(|e| anyhow::anyhow!("send audio failed: {e}"))?;
        Ok(())
    }

    /// Edit a previously sent message. Requires the message ID from send_message_with_id.
    pub async fn edit_message(&self, jid: &str, message_id: &str, new_text: &str) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let new_content = wa::Message {
            conversation: Some(new_text.to_string()),
            ..Default::default()
        };
        client
            .edit_message(target, message_id.to_string(), new_content)
            .await
            .map_err(|e| anyhow::anyhow!("edit failed: {e}"))?;
        Ok(())
    }

    /// Revoke (delete for everyone) a previously sent message.
    pub async fn revoke_message(&self, jid: &str, message_id: &str) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        client
            .revoke_message(
                target,
                message_id.to_string(),
                whatsapp_rust::RevokeType::Sender,
            )
            .await
            .map_err(|e| anyhow::anyhow!("revoke failed: {e}"))?;
        Ok(())
    }

    /// Send a text message and return the message ID (for editing/deleting).
    pub async fn send_message_with_id(&self, jid: &str, text: &str) -> Result<String> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let msg = wa::Message {
            conversation: Some(text.to_string()),
            ..Default::default()
        };
        client
            .send_message(target, msg)
            .await
            .map_err(|e| anyhow::anyhow!("send failed: {e}"))
    }

    /// Send a video file to a chat.
    pub async fn send_video(
        &self,
        jid: &str,
        data: Vec<u8>,
        mime: &str,
        caption: Option<&str>,
    ) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let upload = client
            .upload(data, MediaType::Video)
            .await
            .context("video upload failed")?;
        let msg = wa::Message {
            video_message: Some(Box::new(wa::message::VideoMessage {
                mimetype: Some(mime.to_string()),
                caption: caption.map(|c| c.to_string()),
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
        client
            .send_message(target, msg)
            .await
            .map_err(|e| anyhow::anyhow!("send video failed: {e}"))?;
        Ok(())
    }

    /// Send a document/file to a chat.
    pub async fn send_document(
        &self,
        jid: &str,
        data: Vec<u8>,
        mime: &str,
        filename: &str,
    ) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let upload = client
            .upload(data, MediaType::Document)
            .await
            .context("document upload failed")?;
        let msg = wa::Message {
            document_message: Some(Box::new(wa::message::DocumentMessage {
                mimetype: Some(mime.to_string()),
                file_name: Some(filename.to_string()),
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
        client
            .send_message(target, msg)
            .await
            .map_err(|e| anyhow::anyhow!("send document failed: {e}"))?;
        Ok(())
    }

    /// Send a sticker to a chat. Data should be WebP format.
    pub async fn send_sticker(
        &self,
        jid: &str,
        data: Vec<u8>,
        mime: &str,
        is_animated: bool,
    ) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let upload = client
            .upload(data, MediaType::Sticker)
            .await
            .context("sticker upload failed")?;
        let msg = wa::Message {
            sticker_message: Some(Box::new(wa::message::StickerMessage {
                mimetype: Some(mime.to_string()),
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
        client
            .send_message(target, msg)
            .await
            .map_err(|e| anyhow::anyhow!("send sticker failed: {e}"))?;
        Ok(())
    }

    /// Send an emoji reaction to a message.
    pub async fn send_reaction(
        &self,
        jid: &str,
        target_message_id: &str,
        emoji: &str,
        target_is_from_me: bool,
    ) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let msg = wa::Message {
            reaction_message: Some(wa::message::ReactionMessage {
                key: Some(wa::MessageKey {
                    remote_jid: Some(target.to_string()),
                    id: Some(target_message_id.to_string()),
                    from_me: Some(target_is_from_me),
                    participant: None,
                }),
                text: Some(emoji.to_string()),
                sender_timestamp_ms: Some(chrono::Utc::now().timestamp_millis()),
                ..Default::default()
            }),
            ..Default::default()
        };
        client
            .send_message(target, msg)
            .await
            .map_err(|e| anyhow::anyhow!("send reaction failed: {e}"))?;
        Ok(())
    }

    /// Send a location pin to a chat.
    pub async fn send_location(
        &self,
        jid: &str,
        lat: f64,
        lon: f64,
        name: Option<&str>,
        address: Option<&str>,
    ) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let msg = wa::Message {
            location_message: Some(Box::new(wa::message::LocationMessage {
                degrees_latitude: Some(lat),
                degrees_longitude: Some(lon),
                name: name.map(|n| n.to_string()),
                address: address.map(|a| a.to_string()),
                ..Default::default()
            })),
            ..Default::default()
        };
        client
            .send_message(target, msg)
            .await
            .map_err(|e| anyhow::anyhow!("send location failed: {e}"))?;
        Ok(())
    }

    /// Send a contact card (vCard) to a chat.
    pub async fn send_contact(
        &self,
        jid: &str,
        display_name: &str,
        vcard: &str,
    ) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let msg = wa::Message {
            contact_message: Some(Box::new(wa::message::ContactMessage {
                display_name: Some(display_name.to_string()),
                vcard: Some(vcard.to_string()),
                ..Default::default()
            })),
            ..Default::default()
        };
        client
            .send_message(target, msg)
            .await
            .map_err(|e| anyhow::anyhow!("send contact failed: {e}"))?;
        Ok(())
    }

    /// Send a text message as a reply/quote to another message.
    pub async fn send_reply(
        &self,
        jid: &str,
        reply_to_id: &str,
        reply_to_sender: &str,
        text: &str,
    ) -> Result<String> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let context_info = wa::ContextInfo {
            stanza_id: Some(reply_to_id.to_string()),
            participant: Some(reply_to_sender.to_string()),
            ..Default::default()
        };
        let msg = wa::Message {
            extended_text_message: Some(Box::new(wa::message::ExtendedTextMessage {
                text: Some(text.to_string()),
                context_info: Some(Box::new(context_info)),
                ..Default::default()
            })),
            ..Default::default()
        };
        client
            .send_message(target, msg)
            .await
            .map_err(|e| anyhow::anyhow!("send reply failed: {e}"))
    }
}

// ---------------------------------------------------------------------------
// Bridge runner — reconnection loop with exponential backoff
// ---------------------------------------------------------------------------

async fn run_bridge(
    config: BridgeConfig,
    inbound_tx: mpsc::Sender<WhatsAppInbound>,
    mut outbound_rx: mpsc::Receiver<WhatsAppOutbound>,
    state_tx: watch::Sender<BridgeState>,
    cancel: CancellationToken,
    client_handle: Arc<ParkingMutex<Option<Arc<Client>>>>,
) -> Result<()> {
    info!("WhatsApp bridge starting");

    // Open our lean rusqlite storage backend
    let store = Store::new(&config.db_path).context("failed to open WhatsApp storage database")?;

    // DB integrity check on startup (runs on blocking thread to avoid stalling async executor)
    {
        let store_check = store.clone();
        let check_result = tokio::task::spawn_blocking(move || {
            let conn = store_check.conn_for_check();
            conn.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
        })
        .await
        .context("integrity check task panicked")?;
        match check_result {
            Ok(ref result) if result == "ok" => {
                info!("database integrity check passed");
            }
            Ok(result) => {
                warn!(result = %result, "database integrity check returned warnings");
            }
            Err(e) => {
                error!(error = %e, "database integrity check failed");
            }
        }
    }

    // Message dedup cache survives reconnections
    let dedup = Arc::new(ParkingMutex::new(DedupCache::new(DEDUP_CACHE_CAPACITY)));

    // Secure the database file (Signal Protocol private keys)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&config.db_path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            let _ = std::fs::set_permissions(&config.db_path, perms);
        }
    }

    let mut backoff = Duration::from_secs(1);

    loop {
        if cancel.is_cancelled() {
            break;
        }

        let result = run_bot_session(
            &store,
            &config,
            &inbound_tx,
            &mut outbound_rx,
            &state_tx,
            &cancel,
            &client_handle,
            &dedup,
        )
        .await;

        if cancel.is_cancelled() {
            break;
        }

        match result {
            Ok(SessionAction::Stop) => {
                info!("bot session requested stop; not reconnecting");
                break;
            }
            Ok(SessionAction::Retry) => {
                info!("bot session ended; reconnecting");
                backoff = Duration::from_secs(1); // reset on clean exit
            }
            Err(e) => {
                error!(error = %e, "bot session failed");
            }
        }

        // Exponential backoff with jitter before reconnect (clamped to max)
        let jitter_ms = rand_jitter_ms(backoff.as_millis() as u64);
        let delay = (backoff + Duration::from_millis(jitter_ms)).min(config.max_reconnect_delay);
        let _ = state_tx.send(BridgeState::Reconnecting);
        info!(delay_ms = delay.as_millis() as u64, "reconnecting after delay");

        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = cancel.cancelled() => break,
        }

        backoff = (backoff * 2).min(config.max_reconnect_delay);
    }

    let _ = state_tx.send(BridgeState::Stopped);
    info!("WhatsApp bridge stopped");
    Ok(())
}

// ---------------------------------------------------------------------------
// Single bot session
// ---------------------------------------------------------------------------

async fn run_bot_session(
    store: &Store,
    config: &BridgeConfig,
    inbound_tx: &mpsc::Sender<WhatsAppInbound>,
    outbound_rx: &mut mpsc::Receiver<WhatsAppOutbound>,
    state_tx: &watch::Sender<BridgeState>,
    cancel: &CancellationToken,
    client_handle: &Arc<ParkingMutex<Option<Arc<Client>>>>,
    dedup: &Arc<ParkingMutex<DedupCache>>,
) -> Result<SessionAction> {
    // Clear stale handle from previous session
    set_client_handle(client_handle, None);
    let stop_reconnect = Arc::new(AtomicBool::new(false));

    // Clone store for LID resolution in event handler
    let store_for_events = store.clone();

    // Clones for the event closure
    let ch = client_handle.clone();
    let itx = inbound_tx.clone();
    let stx = Arc::new(state_tx.clone());
    let sr = stop_reconnect.clone();
    let auto_mark_read = config.auto_mark_read;
    let allowed_numbers = config.allowed_numbers.clone();
    let dedup_for_events = dedup.clone();
    let bridge_id = config.bridge_id.clone();

    // Build the Bot
    let mut builder = Bot::builder()
        .with_backend(Arc::new(store.clone()))
        .with_transport_factory(TokioWebSocketTransportFactory::new())
        .with_http_client(UreqHttpClient::new())
        .on_event(move |event, client| {
            let ch = ch.clone();
            let itx = itx.clone();
            let stx = stx.clone();
            let store = store_for_events.clone();
            let sr = sr.clone();
            let allowed = allowed_numbers.clone();
            let dedup = dedup_for_events.clone();
            let bid = bridge_id.clone();
            async move {
                handle_event(event, client, &ch, &itx, &stx, &store, &sr, auto_mark_read, &allowed, &dedup, &bid)
                    .await;
            }
        });

    // Skip history sync to avoid the "deaf client" bug (Issue #125)
    if config.skip_history_sync {
        // NOTE: Uncomment when skip_history_sync() is available in pinned commit (PR #277)
        // builder = builder.skip_history_sync();
    }

    if let Some(ref phone) = config.pair_phone {
        let normalized = normalize_phone(phone);
        info!(phone = %normalized, "pair-code mode enabled");
        builder = builder.with_pair_code(PairCodeOptions {
            phone_number: normalized,
            ..Default::default()
        });
    }

    let mut bot = builder
        .build()
        .await
        .context("failed to build WhatsApp bot")?;
    let bot_task = bot.run().await.context("failed to start WhatsApp bot")?;

    // Run bot + outbound handler + cancellation in parallel
    tokio::select! {
        result = bot_task => {
            match result {
                Ok(()) => {
                    if stop_reconnect.load(Ordering::Relaxed) {
                        Ok(SessionAction::Stop)
                    } else {
                        Ok(SessionAction::Retry)
                    }
                }
                Err(e) => {
                    Err(anyhow::anyhow!("WhatsApp bot task join error: {e}"))
                }
            }
        }
        _ = handle_outbound(outbound_rx, &client_handle, cancel) => {
            if cancel.is_cancelled() || stop_reconnect.load(Ordering::Relaxed) {
                Ok(SessionAction::Stop)
            } else {
                Ok(SessionAction::Retry)
            }
        }
        _ = cancel.cancelled() => {
            info!("bridge shutdown requested");
            if let Some(client) = get_client_handle(&client_handle) {
                client.disconnect().await;
            }
            Ok(SessionAction::Stop)
        }
    }
}

// ---------------------------------------------------------------------------
// Event handling — expanded from 5 to 12+ event types
// ---------------------------------------------------------------------------

async fn handle_event(
    event: Event,
    client: Arc<Client>,
    client_handle: &Arc<ParkingMutex<Option<Arc<Client>>>>,
    inbound_tx: &mpsc::Sender<WhatsAppInbound>,
    state_tx: &Arc<watch::Sender<BridgeState>>,
    store: &Store,
    stop_reconnect: &Arc<AtomicBool>,
    auto_mark_read: bool,
    allowed_numbers: &[String],
    dedup: &Arc<ParkingMutex<DedupCache>>,
    bridge_id: &str,
) {
    match event {
        Event::PairingQrCode { code, .. } => {
            render_qr(&code);
            let _ = state_tx.send(BridgeState::Pairing);
        }
        Event::PairingCode { code, .. } => {
            info!(code = %code, "enter this pair code on your phone");
            let _ = state_tx.send(BridgeState::Pairing);
        }
        Event::Connected(_) => {
            info!("WhatsApp connected");
            set_client_handle(client_handle, Some(client.clone()));
            let _ = state_tx.send(BridgeState::Connected);
        }
        Event::Message(msg, info) => {
            // Dedup: skip messages we've already processed (e.g. after reconnect)
            if dedup.lock().check_and_insert(&info.id) {
                debug!(id = %info.id, "duplicate message, skipping");
                return;
            }

            let sender_raw = info.source.sender.to_string();
            let sender = resolve_sender(&sender_raw, store).await;

            // Allowlist filter: skip messages from numbers not on the list
            if !allowed_numbers.is_empty()
                && !info.source.is_from_me
                && !allowed_numbers.iter().any(|n| n == &sender)
            {
                debug!(sender = %sender, "message from non-allowed sender, skipping");
                return;
            }

            // Extract reply-to context from any message type
            let reply_to = extract_reply_to(&msg);

            // Try to extract content from the message
            let content = extract_content(&msg, &client).await;

            let Some(content) = content else {
                debug!(
                    message_kind = message_kind(&msg),
                    chat = %info.source.chat,
                    sender = %sender,
                    "unhandled inbound message type"
                );
                return;
            };

            let inbound = WhatsAppInbound {
                bridge_id: bridge_id.to_string(),
                jid: info.source.chat.to_string(),
                id: info.id.clone(),
                content,
                sender,
                sender_raw,
                timestamp: info.timestamp.timestamp(),
                reply_to,
                is_from_me: info.source.is_from_me,
            };

            if let Err(e) = inbound_tx.send(inbound).await {
                warn!(error = %e, "inbound channel closed");
            } else if auto_mark_read && !info.source.is_from_me {
                let group_sender = if info.source.is_group {
                    Some(&info.source.sender)
                } else {
                    None
                };
                if let Err(e) = client
                    .mark_as_read(&info.source.chat, group_sender, vec![info.id.clone()])
                    .await
                {
                    warn!(
                        error = %e,
                        chat = %info.source.chat,
                        "failed to send read receipt"
                    );
                }
            }
        }
        Event::LoggedOut(reason) => {
            warn!(reason = ?reason.reason, on_connect = reason.on_connect, "WhatsApp logged out");
            set_client_handle(client_handle, None);
            stop_reconnect.store(true, Ordering::Relaxed);
            request_disconnect(client.clone());
            let _ = state_tx.send(BridgeState::Disconnected);
        }
        Event::StreamError(stream_err) => {
            error!(code = %stream_err.code, "WhatsApp stream error");
            set_client_handle(client_handle, None);
            let _ = state_tx.send(BridgeState::Disconnected);
        }
        Event::StreamReplaced(_) => {
            warn!("session taken over by another device");
            set_client_handle(client_handle, None);
            stop_reconnect.store(true, Ordering::Relaxed);
            request_disconnect(client.clone());
            let _ = state_tx.send(BridgeState::Disconnected);
        }
        Event::TemporaryBan(ban) => {
            error!(?ban.code, ban_expires_secs = ban.expire.num_seconds(), "temporarily banned by WhatsApp");
            set_client_handle(client_handle, None);
            stop_reconnect.store(true, Ordering::Relaxed);
            request_disconnect(client.clone());
            let _ = state_tx.send(BridgeState::Disconnected);
        }
        Event::ClientOutdated(_) => {
            error!("WhatsApp client version outdated — update required");
            set_client_handle(client_handle, None);
            stop_reconnect.store(true, Ordering::Relaxed);
            request_disconnect(client.clone());
            let _ = state_tx.send(BridgeState::Disconnected);
        }
        Event::ConnectFailure(failure) => {
            error!(reason = ?failure.reason, message = %failure.message, "WhatsApp connect failure");
            set_client_handle(client_handle, None);
            if !failure.reason.should_reconnect() {
                stop_reconnect.store(true, Ordering::Relaxed);
                request_disconnect(client.clone());
            }
            let _ = state_tx.send(BridgeState::Disconnected);
        }
        Event::UndecryptableMessage(info) => {
            debug!(?info, "undecryptable message (sender key not yet received)");
        }
        Event::Receipt(receipt) => {
            debug!(
                receipt_type = ?receipt.r#type,
                message_count = receipt.message_ids.len(),
                chat = %receipt.source.chat,
                "receipt update"
            );
        }
        Event::OfflineSyncCompleted(summary) => {
            info!(
                count = summary.count,
                "offline sync completed — real-time messages active"
            );
        }
        _ => {
            debug!(?event, "unhandled event");
        }
    }
}

// ---------------------------------------------------------------------------
// Inbound content extraction
// ---------------------------------------------------------------------------

/// Extract the stanza_id of a quoted/replied-to message, if present.
fn extract_reply_to(msg: &wa::Message) -> Option<String> {
    // Check extended_text_message context
    if let Some(ref ext) = msg.extended_text_message {
        if let Some(ref ctx) = ext.context_info {
            if ctx.stanza_id.is_some() {
                return ctx.stanza_id.clone();
            }
        }
    }
    // Check image_message context
    if let Some(ref img) = msg.image_message {
        if let Some(ref ctx) = img.context_info {
            if ctx.stanza_id.is_some() {
                return ctx.stanza_id.clone();
            }
        }
    }
    // Check video_message context
    if let Some(ref vid) = msg.video_message {
        if let Some(ref ctx) = vid.context_info {
            if ctx.stanza_id.is_some() {
                return ctx.stanza_id.clone();
            }
        }
    }
    // Check audio_message context
    if let Some(ref aud) = msg.audio_message {
        if let Some(ref ctx) = aud.context_info {
            if ctx.stanza_id.is_some() {
                return ctx.stanza_id.clone();
            }
        }
    }
    // Check document_message context
    if let Some(ref doc) = msg.document_message {
        if let Some(ref ctx) = doc.context_info {
            if ctx.stanza_id.is_some() {
                return ctx.stanza_id.clone();
            }
        }
    }
    None
}

/// Maximum recursion depth for unwrapping ephemeral/view-once wrappers.
const MAX_EXTRACT_DEPTH: u8 = 3;

/// Try to extract structured content from an inbound message.
/// Returns None for message types we don't bridge yet.
async fn extract_content(msg: &wa::Message, client: &Arc<Client>) -> Option<InboundContent> {
    extract_content_inner(msg, client, 0).await
}

/// Inner recursive extractor with depth limit.
async fn extract_content_inner(
    msg: &wa::Message,
    client: &Arc<Client>,
    depth: u8,
) -> Option<InboundContent> {
    if depth > MAX_EXTRACT_DEPTH {
        warn!(depth, "extract_content recursion limit reached, skipping");
        return None;
    }
    // --- Text ---
    if let Some(ref text) = msg.conversation {
        return Some(InboundContent::Text {
            body: text.clone(),
        });
    }
    if let Some(ref ext) = msg.extended_text_message {
        if let Some(ref text) = ext.text {
            return Some(InboundContent::Text {
                body: text.clone(),
            });
        }
    }

    // --- Image ---
    if let Some(ref img) = msg.image_message {
        let caption = img.caption.clone();
        let mime = img.mimetype.clone().unwrap_or_else(|| "image/jpeg".to_string());
        match client.download(img.as_ref() as &dyn Downloadable).await {
            Ok(data) => {
                return Some(InboundContent::Image { data, mime, caption });
            }
            Err(e) => {
                warn!(error = %e, "failed to download image, skipping");
                return None;
            }
        }
    }

    // --- Video (includes PTV/video notes) ---
    if let Some(ref vid) = msg.video_message {
        let caption = vid.caption.clone();
        let mime = vid.mimetype.clone().unwrap_or_else(|| "video/mp4".to_string());
        match client.download(vid.as_ref() as &dyn Downloadable).await {
            Ok(data) => {
                return Some(InboundContent::Video { data, mime, caption });
            }
            Err(e) => {
                warn!(error = %e, "failed to download video, skipping");
                return None;
            }
        }
    }

    // --- Audio ---
    if let Some(ref aud) = msg.audio_message {
        let mime = aud.mimetype.clone().unwrap_or_else(|| "audio/ogg".to_string());
        let seconds = aud.seconds;
        let is_voice = aud.ptt.unwrap_or(false);
        match client.download(aud.as_ref() as &dyn Downloadable).await {
            Ok(data) => {
                return Some(InboundContent::Audio { data, mime, seconds, is_voice });
            }
            Err(e) => {
                warn!(error = %e, "failed to download audio, skipping");
                return None;
            }
        }
    }

    // --- Document ---
    if let Some(ref doc) = msg.document_message {
        let mime = doc.mimetype.clone().unwrap_or_else(|| "application/octet-stream".to_string());
        let filename = doc.file_name.clone().unwrap_or_else(|| "file".to_string());
        match client.download(doc.as_ref() as &dyn Downloadable).await {
            Ok(data) => {
                return Some(InboundContent::Document { data, mime, filename });
            }
            Err(e) => {
                warn!(error = %e, "failed to download document, skipping");
                return None;
            }
        }
    }

    // --- Document with caption (wrapper) ---
    if let Some(ref dwc) = msg.document_with_caption_message {
        if let Some(ref inner) = dwc.message {
            if let Some(ref doc) = inner.document_message {
                let mime = doc.mimetype.clone().unwrap_or_else(|| "application/octet-stream".to_string());
                let filename = doc.file_name.clone().unwrap_or_else(|| "file".to_string());
                match client.download(doc.as_ref() as &dyn Downloadable).await {
                    Ok(data) => {
                        return Some(InboundContent::Document { data, mime, filename });
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to download document (with caption), skipping");
                        return None;
                    }
                }
            }
        }
    }

    // --- Sticker ---
    if let Some(ref stk) = msg.sticker_message {
        let mime = stk.mimetype.clone().unwrap_or_else(|| "image/webp".to_string());
        let is_animated = stk.is_animated.unwrap_or(false);
        match client.download(stk.as_ref() as &dyn Downloadable).await {
            Ok(data) => {
                return Some(InboundContent::Sticker { data, mime, is_animated });
            }
            Err(e) => {
                warn!(error = %e, "failed to download sticker, skipping");
                return None;
            }
        }
    }

    // --- Location ---
    if let Some(ref loc) = msg.location_message {
        if let (Some(lat), Some(lon)) = (loc.degrees_latitude, loc.degrees_longitude) {
            return Some(InboundContent::Location {
                lat,
                lon,
                name: loc.name.clone(),
                address: loc.address.clone(),
            });
        }
    }

    // --- Live location (bridge as static snapshot) ---
    if let Some(ref loc) = msg.live_location_message {
        if let (Some(lat), Some(lon)) = (loc.degrees_latitude, loc.degrees_longitude) {
            return Some(InboundContent::Location {
                lat,
                lon,
                name: loc.caption.clone(),
                address: None,
            });
        }
    }

    // --- Contact ---
    if let Some(ref contact) = msg.contact_message {
        return Some(InboundContent::Contact {
            display_name: contact.display_name.clone().unwrap_or_default(),
            vcard: contact.vcard.clone().unwrap_or_default(),
        });
    }

    // --- Multiple contacts ---
    if let Some(ref arr) = msg.contacts_array_message {
        // Bridge as first contact (most common case is single)
        if let Some(first) = arr.contacts.first() {
            return Some(InboundContent::Contact {
                display_name: first.display_name.clone().unwrap_or_else(|| {
                    arr.display_name.clone().unwrap_or_default()
                }),
                vcard: first.vcard.clone().unwrap_or_default(),
            });
        }
    }

    // --- Reaction ---
    if let Some(ref reaction) = msg.reaction_message {
        if let Some(ref key) = reaction.key {
            return Some(InboundContent::Reaction {
                target_id: key.id.clone().unwrap_or_default(),
                emoji: reaction.text.clone().unwrap_or_default(),
            });
        }
    }

    // --- Edit (incoming edit from others) ---
    if let Some(ref edited) = msg.edited_message {
        if let Some(ref inner) = edited.message {
            if let Some(ref proto) = inner.protocol_message {
                let target_id = proto
                    .key
                    .as_ref()
                    .and_then(|k| k.id.clone())
                    .unwrap_or_default();
                let new_text = proto
                    .edited_message
                    .as_ref()
                    .and_then(|m| {
                        m.conversation.clone().or_else(|| {
                            m.extended_text_message
                                .as_ref()
                                .and_then(|e| e.text.clone())
                        })
                    })
                    .unwrap_or_default();
                return Some(InboundContent::Edit { target_id, new_text });
            }
        }
    }

    // --- Protocol message (revokes / deletes) ---
    if let Some(ref proto) = msg.protocol_message {
        // Revoke = type 0 (REVOKE) with a key
        if proto.r#type == Some(0) {
            if let Some(ref key) = proto.key {
                return Some(InboundContent::Revoke {
                    target_id: key.id.clone().unwrap_or_default(),
                });
            }
        }
    }

    // --- Ephemeral wrapper (unwrap and recurse) ---
    if let Some(ref eph) = msg.ephemeral_message {
        if let Some(ref inner) = eph.message {
            return Box::pin(extract_content_inner(inner, client, depth + 1)).await;
        }
    }

    // --- View-once wrapper (unwrap and recurse) ---
    if let Some(ref vo) = msg.view_once_message {
        if let Some(ref inner) = vo.message {
            return Box::pin(extract_content_inner(inner, client, depth + 1)).await;
        }
    }
    if let Some(ref vo2) = msg.view_once_message_v2 {
        if let Some(ref inner) = vo2.message {
            return Box::pin(extract_content_inner(inner, client, depth + 1)).await;
        }
    }

    None
}

// ---------------------------------------------------------------------------
// LID-to-phone sender resolution
// ---------------------------------------------------------------------------

/// Resolve a sender JID to a phone number using the LID mapping table.
/// Falls back to the raw JID string if no mapping exists.
async fn resolve_sender(sender_raw: &str, store: &Store) -> String {
    // Extract the user part (before @)
    let user_part = sender_raw
        .split('@')
        .next()
        .unwrap_or(sender_raw)
        .to_string();

    // Try LID → phone lookup
    if let Ok(Some(entry)) = store.get_lid_mapping(&user_part).await {
        return entry.phone_number;
    }

    // Already a phone number or no mapping — return as-is
    user_part
}

// ---------------------------------------------------------------------------
// Outbound message handling
// ---------------------------------------------------------------------------

async fn handle_outbound(
    outbound_rx: &mut mpsc::Receiver<WhatsAppOutbound>,
    client_handle: &Arc<ParkingMutex<Option<Arc<Client>>>>,
    cancel: &CancellationToken,
) {
    let mut pending: VecDeque<WhatsAppOutbound> = VecDeque::new();
    let mut outbound_closed = false;

    loop {
        if cancel.is_cancelled() {
            break;
        }

        while !outbound_closed {
            match outbound_rx.try_recv() {
                Ok(out) => {
                    if pending.len() >= MAX_OUTBOUND_RETRY_QUEUE {
                        let dropped = pending.pop_front();
                        if let Some(d) = dropped {
                            error!(jid = %d.jid, "dropping oldest outbound after queue reached capacity");
                        }
                    }
                    pending.push_back(out);
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    outbound_closed = true;
                    break;
                }
            }
        }

        if pending.is_empty() {
            if outbound_closed {
                break;
            }
            tokio::select! {
                _ = cancel.cancelled() => break,
                msg = outbound_rx.recv() => {
                    match msg {
                        Some(out) => pending.push_back(out),
                        None => outbound_closed = true,
                    }
                }
            }
            continue;
        }

        let Some(client) = get_client_handle(client_handle) else {
            tokio::select! {
                _ = tokio::time::sleep(OUTBOUND_RETRY_DELAY) => {}
                _ = cancel.cancelled() => break,
            }
            continue;
        };

        while let Some(out) = pending.pop_front() {
            let jid = match parse_jid(&out.jid) {
                Ok(j) => j,
                Err(e) => {
                    error!(error = %e, jid = %out.jid, "invalid outbound JID");
                    continue;
                }
            };

            let msg = wa::Message {
                conversation: Some(out.text.clone()),
                ..Default::default()
            };

            match client.send_message(jid, msg).await {
                Ok(_msg_id) => {}
                Err(e) => {
                    let mut out = out;
                    out.retries += 1;
                    if out.retries >= MAX_OUTBOUND_RETRIES {
                        error!(
                            error = %e, jid = %out.jid, retries = out.retries,
                            "dropping outbound message after max retries"
                        );
                    } else {
                        warn!(
                            error = %e, jid = %out.jid, retry = out.retries,
                            "failed to send outbound message; will retry"
                        );
                        // Push to back (not front) to avoid head-of-line blocking
                        pending.push_back(out);
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(OUTBOUND_RETRY_DELAY) => {}
                        _ = cancel.cancelled() => break,
                    }
                    break;
                }
            }
        }

        if outbound_closed && pending.is_empty() {
            break;
        }
    }

    if !pending.is_empty() {
        warn!(
            pending_count = pending.len(),
            "outbound handler stopped with unsent messages"
        );
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a JID string (phone@s.whatsapp.net or raw phone number).
/// Handles formatted phone numbers like "+1 (555) 123-4567" → "15551234567@s.whatsapp.net".
fn parse_jid(s: &str) -> Result<Jid> {
    if s.contains('@') {
        Jid::from_str(s).map_err(|e| anyhow::anyhow!("bad JID: {e}"))
    } else {
        let normalized = normalize_phone(s);
        if normalized.is_empty() {
            return Err(anyhow::anyhow!("empty JID after normalization: {s}"));
        }
        // Newsletter JIDs start with 120363; all others are individual chats
        let server = if normalized.starts_with("120363") {
            "g.us"
        } else {
            "s.whatsapp.net"
        };
        Jid::from_str(&format!("{normalized}@{server}"))
            .map_err(|e| anyhow::anyhow!("bad JID from phone: {e}"))
    }
}

fn message_kind(msg: &wa::Message) -> &'static str {
    if msg.image_message.is_some() {
        "image"
    } else if msg.video_message.is_some() || msg.ptv_message.is_some() {
        "video"
    } else if msg.audio_message.is_some() {
        "audio"
    } else if msg.document_message.is_some() || msg.document_with_caption_message.is_some() {
        "document"
    } else if msg.reaction_message.is_some() || msg.enc_reaction_message.is_some() {
        "reaction"
    } else if msg.edited_message.is_some() {
        "edit"
    } else if msg.protocol_message.is_some() {
        "protocol"
    } else if msg.poll_creation_message.is_some()
        || msg.poll_creation_message_v2.is_some()
        || msg.poll_creation_message_v3.is_some()
        || msg.poll_creation_message_v4.is_some()
        || msg.poll_update_message.is_some()
    {
        "poll"
    } else if msg.ephemeral_message.is_some() {
        "ephemeral"
    } else if msg.view_once_message.is_some()
        || msg.view_once_message_v2.is_some()
        || msg.view_once_message_v2_extension.is_some()
    {
        "view_once"
    } else if msg.location_message.is_some() || msg.live_location_message.is_some() {
        "location"
    } else if msg.contact_message.is_some() || msg.contacts_array_message.is_some() {
        "contact"
    } else {
        "other"
    }
}

fn set_client_handle(
    client_handle: &Arc<ParkingMutex<Option<Arc<Client>>>>,
    value: Option<Arc<Client>>,
) {
    *client_handle.lock() = value;
}

fn get_client_handle(
    client_handle: &Arc<ParkingMutex<Option<Arc<Client>>>>,
) -> Option<Arc<Client>> {
    client_handle.lock().clone()
}

fn request_disconnect(client: Arc<Client>) {
    tokio::spawn(async move {
        client.disconnect().await;
    });
}

/// Normalize a phone number to digits only (E.164 without the +).
/// Strips all non-digit characters: "+1 (555) 123-4567" → "15551234567".
fn normalize_phone(raw: &str) -> String {
    raw.chars().filter(|c| c.is_ascii_digit()).collect()
}

/// Generate a random jitter in [0, base_ms/2) using a simple xorshift on the system time.
/// No external RNG crate needed — jitter doesn't need cryptographic quality.
fn rand_jitter_ms(base_ms: u64) -> u64 {
    if base_ms == 0 {
        return 0;
    }
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    // xorshift64
    seed ^= seed << 13;
    seed ^= seed >> 7;
    seed ^= seed << 17;
    seed % (base_ms / 2).max(1)
}

/// Render a QR code in the terminal using Unicode block characters.
fn render_qr(data: &str) {
    use qrcode::QrCode;

    let Ok(code) = QrCode::new(data.as_bytes()) else {
        error!("failed to generate QR code");
        return;
    };

    let string = code
        .render::<char>()
        .quiet_zone(true)
        .module_dimensions(2, 1)
        .build();

    println!("\n{string}\n");
    info!("scan the QR code above with WhatsApp on your phone");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_phone() {
        assert_eq!(normalize_phone("15551234567"), "15551234567");
        assert_eq!(normalize_phone("+1 (555) 123-4567"), "15551234567");
        assert_eq!(normalize_phone("+44 20 7946 0958"), "442079460958");
        assert_eq!(normalize_phone(""), "");
    }

    #[test]
    fn test_parse_jid_with_server() {
        let jid = parse_jid("15551234567@s.whatsapp.net").unwrap();
        assert_eq!(jid.to_string(), "15551234567@s.whatsapp.net");
    }

    #[test]
    fn test_parse_jid_digits_only() {
        let jid = parse_jid("15551234567").unwrap();
        assert!(jid.to_string().contains("s.whatsapp.net"));
    }

    #[test]
    fn test_parse_jid_formatted_phone() {
        let jid = parse_jid("+1 (555) 123-4567").unwrap();
        assert!(jid.to_string().starts_with("15551234567@"));
    }

    #[test]
    fn test_parse_jid_newsletter() {
        let jid = parse_jid("120363012345678901@g.us").unwrap();
        assert!(jid.to_string().contains("g.us"));
    }

    #[test]
    fn test_parse_jid_empty_rejects() {
        assert!(parse_jid("+++").is_err());
    }

    #[test]
    fn test_message_kind_default() {
        let msg = wa::Message::default();
        assert_eq!(message_kind(&msg), "other");
    }

    #[test]
    fn test_dedup_cache() {
        let mut cache = DedupCache::new(3);
        assert!(!cache.check_and_insert("a")); // first time → false
        assert!(cache.check_and_insert("a"));  // duplicate → true
        assert!(!cache.check_and_insert("b"));
        assert!(!cache.check_and_insert("c")); // cache: [a, b, c] (full)
        // Inserting "d" evicts "a"
        assert!(!cache.check_and_insert("d")); // cache: [b, c, d]
        assert!(!cache.check_and_insert("a")); // "a" was evicted → false; cache: [c, d, a]
        assert!(cache.check_and_insert("d"));  // "d" still in cache → true
        assert!(cache.check_and_insert("c"));  // "c" still in cache → true
    }

    #[test]
    fn test_rand_jitter_ms() {
        // Zero base should return 0
        assert_eq!(rand_jitter_ms(0), 0);
        // Should return less than half of base
        for _ in 0..100 {
            let j = rand_jitter_ms(10000);
            assert!(j < 5000, "jitter {j} should be < 5000");
        }
    }

    #[test]
    fn test_allowlist_logic() {
        let allowed = vec!["15551234567".to_string(), "442079460958".to_string()];
        // In-list passes
        assert!(allowed.iter().any(|n| n == "15551234567"));
        // Not-in-list blocks
        assert!(!allowed.iter().any(|n| n == "19998887777"));
        // Empty list → no filter (pass all)
        let empty: Vec<String> = vec![];
        assert!(empty.is_empty());
    }
}
